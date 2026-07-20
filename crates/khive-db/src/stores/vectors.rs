//! sqlite-vec backed `VectorStore`: one vec0 table per embedding model, scoped to namespace.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use uuid::Uuid;

use khive_score::DeterministicScore;
use khive_storage::error::StorageError;
use khive_storage::types::{
    BatchWriteSummary, IndexRebuildScope, OrphanSweepConfig, OrphanSweepResult, SqlStatement,
    SqlValue, VectorIndexKind, VectorRecord, VectorSearchHit, VectorSearchRequest,
    VectorStoreCapabilities, VectorStoreInfo,
};
use khive_storage::StorageCapability;
use khive_storage::StorageResult;
use khive_storage::VectorStore;
use khive_types::SubstrateKind;

use crate::error::SqliteError;
use crate::pool::ConnectionPool;
use crate::sql_bridge::bind_params;

/// The exact `DELETE` this store's `delete` issues, for a given vector table
/// (ADR-099 B3 r6 structural cut — see `stores::entity`'s sibling block).
/// `table` must already be a trusted, sanitized table name (mirrors
/// `delete`'s own pre-existing lack of a placeholder for table names).
pub fn delete_vector_statement(table: &str, subject_id: Uuid, namespace: &str) -> SqlStatement {
    SqlStatement {
        sql: format!("DELETE FROM {table} WHERE subject_id = ?1 AND namespace = ?2"),
        params: vec![
            SqlValue::Text(subject_id.to_string()),
            SqlValue::Text(namespace.to_string()),
        ],
        label: Some(format!("vec-delete-{table}")),
    }
}

// ---------------------------------------------------------------------------
// Test-only failpoint: force an error between DELETE and INSERT to exercise
// the SAVEPOINT ROLLBACK TO path in insert_batch and the transaction rollback
// in update.  Zero impact on release builds — the entire block is cfg(test).
//
// Uses Arc<AtomicBool> rather than thread_local! because the actual DB work
// runs inside tokio::task::spawn_blocking on a worker thread different from
// the test thread.  The Arc is cloned into the closure so both sides share
// the same flag without a thread boundary problem.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod failpoint {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use std::cell::RefCell;

    thread_local! {
        /// Per-test handle to the shared AtomicBool.  Each test that needs
        /// the failpoint calls `arm()` to create a fresh Arc and store it here;
        /// the `FailpointGuard` clears it on drop.
        pub(super) static CURRENT: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
    }

    // The arming mechanism (`arm`/`disarm`/`FailpointGuard`) is used only by the
    // SAVEPOINT/ROLLBACK sentinel tests, which need the sqlite-vec store and so
    // live in the `cfg(all(test, feature = "vectors"))` module below.  Gating
    // these items on `feature = "vectors"` keeps them out of the no-feature test
    // build, where they would otherwise have no caller and trip
    // `clippy --all-targets -D warnings` (which runs without `--features vectors`).
    // `CURRENT`/`take` stay plain `cfg(test)`: they are read by the failpoint hooks
    // in `insert_batch`/`update`, which are `cfg(test)` and compile in every test build.

    /// Create a fresh `Arc<AtomicBool>` set to `true` and register it in the
    /// thread-local so the write closure can capture it before spawn_blocking.
    #[cfg(feature = "vectors")]
    pub(super) fn arm() {
        let flag = Arc::new(AtomicBool::new(true));
        CURRENT.with(|c| *c.borrow_mut() = Some(flag));
    }

    /// Disarm: clear the thread-local (the Arc may live on in the closure
    /// a moment longer, but the flag is already spent after one `take()`).
    #[cfg(feature = "vectors")]
    pub(super) fn disarm() {
        CURRENT.with(|c| *c.borrow_mut() = None);
    }

    /// Called from inside the write closure (worker thread).
    /// Atomically swaps `true` → `false` and returns whether it fired.
    pub(super) fn take(flag: &Arc<AtomicBool>) -> bool {
        flag.compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// RAII guard: arms the failpoint on construction and disarms on drop.
    /// The Arc is stored in the thread-local and captured by the write closure
    /// directly; the guard's only job is to ensure `disarm()` runs on drop.
    #[cfg(feature = "vectors")]
    pub(super) struct FailpointGuard;

    #[cfg(feature = "vectors")]
    impl FailpointGuard {
        pub(super) fn new() -> Self {
            arm();
            Self
        }
    }

    #[cfg(feature = "vectors")]
    impl Drop for FailpointGuard {
        fn drop(&mut self) {
            disarm();
        }
    }
}

/// Cast a `&[f32]` slice to `&[u8]` for sqlite-vec blob binding.
///
/// # Safety
///
/// Safe: f32 has no alignment requirements beyond what &[u8] needs, the byte
/// length is exactly the input slice size, and the lifetime is tied to input.
fn f32_slice_as_bytes(data: &[f32]) -> &[u8] {
    // SAFETY: `data` is a valid &[f32] so the pointer is non-null, well-aligned, and
    // live for the call duration. u8 alignment is 1 (satisfied by any allocation).
    // size_of_val gives the exact byte count. The returned slice borrows `data`
    // so its lifetime cannot outlive the input reference.
    unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, std::mem::size_of_val(data)) }
}

/// Snapshot the current thread's failpoint flag (test builds only; always
/// `None` in a release build). Exists so `insert_batch` can capture the
/// thread-local's value once, unconditionally, before choosing between the
/// flag-on (WriterTask) and flag-off (legacy pool-mutex) write paths —
/// both eventually move the captured `Option` into a `spawn_blocking`
/// closure on a different thread than the one that read the thread-local.
#[cfg(test)]
fn current_failpoint() -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
    failpoint::CURRENT.with(|c| c.borrow().clone())
}

#[cfg(not(test))]
fn current_failpoint() -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
    None
}

fn map_err(e: rusqlite::Error, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Vectors, op, e)
}

fn map_sqlite_err(e: SqliteError, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Vectors, op, e)
}

fn non_finite_index(data: &[f32]) -> Option<usize> {
    data.iter().position(|v| !v.is_finite())
}

fn non_finite_vector_error(op: &'static str, idx: usize, value: f32) -> StorageError {
    StorageError::InvalidInput {
        capability: StorageCapability::Vectors,
        operation: op.into(),
        message: format!(
            "non-finite value at index {idx}: {value} \
             (NaN/Inf values corrupt distance computations)"
        ),
    }
}

/// Validate that `model_key` is safe to interpolate into a SQLite table name.
fn validate_model_key(model_key: &str) -> Result<(), SqliteError> {
    if model_key.is_empty()
        || !model_key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(SqliteError::InvalidData(format!(
            "invalid model_key '{}': must be non-empty and contain only ASCII alphanumeric/underscore characters",
            model_key
        )));
    }
    Ok(())
}

/// A VectorStore backed by sqlite-vec's vec0 virtual tables.
///
/// Each instance manages one table `vec_{model_key}`. The `namespace` field
/// is a default for trait methods that lack a per-call namespace parameter
/// (count, delete, info). Access control is enforced at the runtime layer.
pub struct SqliteVecStore {
    pool: Arc<ConnectionPool>,
    is_file_backed: bool,
    model_key: String,
    embedding_model: String,
    dimensions: usize,
    table_name: String,
    namespace: String,
    writer_task: Option<crate::writer_task::WriterTaskHandle>,
}

impl SqliteVecStore {
    /// Create a new store scoped to the given namespace.
    ///
    /// Returns an error if `model_key` contains characters unsafe for table name interpolation.
    pub fn new(
        pool: Arc<ConnectionPool>,
        is_file_backed: bool,
        model_key: String,
        embedding_model: String,
        dimensions: usize,
        namespace: String,
    ) -> Result<Self, SqliteError> {
        validate_model_key(&model_key)?;
        let table_name = format!("vec_{}", model_key);
        // Best-effort opt-in (ADR-067 Component A, mirrors entity.rs slice 1
        // policy): a missing writer task degrades to the legacy pool-mutex
        // path rather than failing construction.
        let writer_task = pool.writer_task_handle().ok().flatten();
        Ok(Self {
            pool,
            is_file_backed,
            model_key,
            embedding_model,
            dimensions,
            table_name,
            namespace,
            writer_task,
        })
    }

    fn open_standalone_reader(&self) -> Result<rusqlite::Connection, StorageError> {
        let config = self.pool.config();
        let path = config.path.as_ref().ok_or_else(|| StorageError::Pool {
            operation: "vec_reader".into(),
            message: "in-memory databases do not support standalone connections".into(),
        })?;

        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| map_err(e, "open_vec_reader"))?;

        conn.busy_timeout(config.busy_timeout)
            .map_err(|e| map_err(e, "open_vec_reader"))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| map_err(e, "open_vec_reader"))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| map_err(e, "open_vec_reader"))?;

        Ok(conn)
    }

    /// Route a single-row DML-only write through the pool-wide `WriterTask`
    /// when available, else fall back to `with_writer_unmanaged`. See
    /// crates/khive-db/docs/api/vectors.md#with_writer--with_writer_unmanaged--writertask-routing-adr-067-component-a-fork-c-slice-2
    async fn with_writer<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        if let Some(writer_task) = &self.writer_task {
            return writer_task
                .send(move |conn| f(conn).map_err(|e| map_err(e, op)))
                .await;
        }

        self.with_writer_unmanaged(op, f).await
    }

    /// Legacy pool-mutex write path; bypasses the WriterTask channel
    /// unconditionally. Reserved for closures that manage their own
    /// transaction. See
    /// crates/khive-db/docs/api/vectors.md#with_writer--with_writer_unmanaged--writertask-routing-adr-067-component-a-fork-c-slice-2
    async fn with_writer_unmanaged<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let guard = pool.try_writer().map_err(|e| map_sqlite_err(e, op))?;
            f(guard.conn()).map_err(|e| map_err(e, op))
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Vectors, op, e))?
    }

    async fn with_reader<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        if self.is_file_backed {
            let conn = self.open_standalone_reader()?;
            tokio::task::spawn_blocking(move || f(&conn).map_err(|e| map_err(e, op)))
                .await
                .map_err(|e| StorageError::driver(StorageCapability::Vectors, op, e))?
        } else {
            let pool = Arc::clone(&self.pool);
            tokio::task::spawn_blocking(move || {
                let guard = pool.reader().map_err(|e| map_sqlite_err(e, op))?;
                f(guard.conn()).map_err(|e| map_err(e, op))
            })
            .await
            .map_err(|e| StorageError::driver(StorageCapability::Vectors, op, e))?
        }
    }
}

/// One vector row's identity + payload for [`replace_vector_row_dml`] (#546).
/// `embedding` must already be validated for the target table's dimension
/// count (or delegated to the helper's own dimension check).
struct VectorRowRef<'a> {
    subject_id: Uuid,
    namespace: &'a str,
    kind: &'a str,
    field: &'a str,
    embedding_model: &'a str,
    embedding: &'a [f32],
}

/// Shared DELETE-then-INSERT replacement DML for a single vector row (#546);
/// caller owns the enclosing transaction/savepoint. See
/// crates/khive-db/docs/api/vectors.md#replace_vector_row_dml--shared-delete-then-insert-replacement-546
fn replace_vector_row_dml(
    conn: &rusqlite::Connection,
    table: &str,
    dims: usize,
    row: VectorRowRef<'_>,
    failpoint_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<(), rusqlite::Error> {
    if row.embedding.len() != dims {
        return Err(rusqlite::Error::InvalidParameterCount(
            row.embedding.len(),
            dims,
        ));
    }

    let del_sql = format!("DELETE FROM {table} WHERE subject_id = ?1 AND namespace = ?2");
    conn.execute(
        &del_sql,
        rusqlite::params![row.subject_id.to_string(), row.namespace],
    )?;

    // Failpoint: fires only in cfg(test) when the guard is active. DELETE has
    // already run; if the caller's rollback (transaction or SAVEPOINT) is
    // missing, the deleted row is lost permanently.
    #[cfg(test)]
    if let Some(ref fp) = failpoint_flag {
        if failpoint::take(fp) {
            return Err(rusqlite::Error::InvalidParameterName(
                "__test_failpoint_after_delete__".into(),
            ));
        }
    }
    #[cfg(not(test))]
    let _ = failpoint_flag;

    let ins_sql = format!(
        "INSERT INTO {table} (subject_id, namespace, kind, field, embedding_model, embedding) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
    );
    let blob = f32_slice_as_bytes(row.embedding);
    conn.execute(
        &ins_sql,
        rusqlite::params![
            row.subject_id.to_string(),
            row.namespace,
            row.kind,
            row.field,
            row.embedding_model,
            blob
        ],
    )?;

    // Delta record for the ANN restart classifier; rides the caller's
    // savepoint/transaction so a rolled-back upsert leaves no log row.
    conn.execute(
        "INSERT INTO ann_write_log (namespace, embedding_model, kind, field, subject_id, op) \
         VALUES (?1, ?2, ?3, ?4, ?5, 'upsert')",
        rusqlite::params![
            row.namespace,
            row.embedding_model,
            row.kind,
            row.field,
            row.subject_id.to_string()
        ],
    )?;

    Ok(())
}

/// Log `'delete'` rows into `ann_write_log` for every vector row in `table`
/// matching `where_clause` (a predicate over the vec0 table's own columns).
/// Must run in the same transaction as — and before — the corresponding
/// `DELETE`, so the logged set is exactly the deleted set.
fn log_vector_deletes(
    conn: &rusqlite::Connection,
    table: &str,
    where_clause: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<(), rusqlite::Error> {
    let sql = format!(
        "INSERT INTO ann_write_log (namespace, embedding_model, kind, field, subject_id, op) \
         SELECT namespace, embedding_model, kind, field, subject_id, 'delete' \
         FROM {table} WHERE {where_clause}"
    );
    conn.execute(&sql, params)?;
    Ok(())
}

/// Delete `subject_id`'s row from every registered-model vector table, in
/// `namespace` (#546).
///
/// Shared by runtime curation's entity/note merge cleanup, which must sweep
/// the merged-away subject out of every model's `vec_{model_key}` table, not
/// just the primary embedding model's. Callers own the enclosing transaction;
/// this issues no `BEGIN`/`COMMIT`/`SAVEPOINT`.
pub fn delete_subject_from_vector_tables(
    conn: &rusqlite::Connection,
    tables: &[String],
    subject_id: Uuid,
    namespace: &str,
) -> Result<(), rusqlite::Error> {
    for table in tables {
        log_vector_deletes(
            conn,
            table,
            "subject_id = ?1 AND namespace = ?2",
            &[&subject_id.to_string(), &namespace],
        )?;
        let sql = format!("DELETE FROM {table} WHERE subject_id = ?1 AND namespace = ?2");
        conn.execute(&sql, rusqlite::params![subject_id.to_string(), namespace])?;
    }
    Ok(())
}

/// DML-only batch insert loop shared by both the legacy (flag-off) and
/// WriterTask-routed (flag-on) `insert_batch` paths (ADR-067 Component A).
///
/// Issues no OUTER `BEGIN` / `COMMIT` / `ROLLBACK` — the caller owns the
/// enclosing transaction. The per-record named `SAVEPOINT vec_batch_record`
/// is preserved unchanged: it gives a failed INSERT a no-worse-than-stale
/// rollback (only that record's DELETE is undone) independent of which
/// outer transaction wraps the loop.
#[allow(clippy::too_many_arguments)]
fn batch_insert_vectors_dml(
    conn: &rusqlite::Connection,
    table: &str,
    dims: usize,
    store_embedding_model: &str,
    records: &[VectorRecord],
    attempted: u64,
    failpoint_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<BatchWriteSummary, rusqlite::Error> {
    let mut affected = 0u64;
    let mut failed = 0u64;
    let mut first_error = String::new();

    for record in records {
        if record.vectors.len() != 1 {
            if first_error.is_empty() {
                first_error = format!("expected 1 vector per record, got {}", record.vectors.len());
            }
            failed += 1;
            continue;
        }
        let embedding = &record.vectors[0];
        if embedding.len() != dims {
            if first_error.is_empty() {
                first_error = format!(
                    "wrong vector dimension: expected {dims}, got {}",
                    embedding.len()
                );
            }
            failed += 1;
            continue;
        }
        if non_finite_index(embedding).is_some() {
            if first_error.is_empty() {
                first_error = "embedding contains non-finite values (NaN or Inf)".to_string();
            }
            failed += 1;
            continue;
        }
        let kind_str = record.kind.to_string();

        // Wrap each record's DELETE+INSERT in a savepoint so a failed INSERT
        // rolls back only that record's DELETE, leaving the prior vector intact
        // (no-worse-than-stale guarantee, same as single-record `insert`).
        conn.execute_batch("SAVEPOINT vec_batch_record")?;
        let result = replace_vector_row_dml(
            conn,
            table,
            dims,
            VectorRowRef {
                subject_id: record.subject_id,
                namespace: &record.namespace,
                kind: &kind_str,
                field: &record.field,
                embedding_model: store_embedding_model,
                embedding,
            },
            failpoint_flag.clone(),
        );
        match result {
            Ok(()) => {
                conn.execute_batch("RELEASE SAVEPOINT vec_batch_record")?;
                affected += 1;
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK TO SAVEPOINT vec_batch_record");
                let _ = conn.execute_batch("RELEASE SAVEPOINT vec_batch_record");
                if first_error.is_empty() {
                    first_error = e.to_string();
                }
                failed += 1;
            }
        }
    }

    Ok(BatchWriteSummary {
        attempted,
        affected,
        failed,
        first_error,
    })
}

/// Shared DELETE-then-INSERT DML for single-record `insert`/`update`, run
/// inside a named `SAVEPOINT` (nestable inside the WriterTask's own
/// transaction) instead of `conn.unchecked_transaction()` (which would
/// attempt a nested `BEGIN` and fail once this runs inside the WriterTask's
/// already-open transaction). A failed INSERT rolls back only this
/// SAVEPOINT, leaving the previous vector intact (no-worse-than-stale
/// guarantee) — the single-record analog of `batch_insert_vectors_dml`'s
/// per-record `SAVEPOINT vec_batch_record`.
#[allow(clippy::too_many_arguments)]
fn vec_upsert_atomic_dml(
    conn: &rusqlite::Connection,
    table: &str,
    dims: usize,
    subject_id: Uuid,
    kind_str: &str,
    namespace: &str,
    field: &str,
    embedding_model: &str,
    embedding: &[f32],
    savepoint_name: &'static str,
    failpoint_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<(), rusqlite::Error> {
    conn.execute_batch(&format!("SAVEPOINT {savepoint_name}"))?;
    let result = replace_vector_row_dml(
        conn,
        table,
        dims,
        VectorRowRef {
            subject_id,
            namespace,
            kind: kind_str,
            field,
            embedding_model,
            embedding,
        },
        failpoint_flag,
    );

    match result {
        Ok(()) => {
            conn.execute_batch(&format!("RELEASE SAVEPOINT {savepoint_name}"))?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch(&format!("ROLLBACK TO SAVEPOINT {savepoint_name}"));
            let _ = conn.execute_batch(&format!("RELEASE SAVEPOINT {savepoint_name}"));
            Err(e)
        }
    }
}

/// DML-only orphan-sweep body shared by both the legacy (flag-off) and
/// WriterTask-routed (flag-on) `orphan_sweep` paths (ADR-067 Amendment 1).
///
/// Issues no `BEGIN` / `COMMIT` / `ROLLBACK` — the caller owns the enclosing
/// transaction (either the flag-off path's `Transaction::new_unchecked`, or
/// the WriterTask drain loop's own `BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK`
/// wrap). `ns_json` / `kind_json` / `allow_json` are the pre-serialized JSON
/// filter arguments (or `None` for "no filter") computed once by the caller.
fn orphan_sweep_dml(
    conn: &rusqlite::Connection,
    table: &str,
    ns_json: Option<&str>,
    kind_json: Option<&str>,
    allow_json: Option<&str>,
    max_delete: i64,
    dry_run: bool,
) -> Result<OrphanSweepResult, rusqlite::Error> {
    // Optional-filter clause shared across all three queries.
    // Each ?N appears twice (IS NULL guard + json_each call); SQLite
    // reuses the same bound value for every occurrence of the same ?N.
    //   ?1 = namespace JSON or NULL   ?2 = kind JSON or NULL
    //   ?3 = allowlist JSON or NULL
    let filter_pred = "(?1 IS NULL OR namespace IN (SELECT value FROM json_each(?1))) \
                       AND (?2 IS NULL OR kind IN (SELECT value FROM json_each(?2))) \
                       AND (?3 IS NULL OR subject_id IN (SELECT value FROM json_each(?3)))";

    // Live-subjects subquery used in the orphan anti-join.
    //
    // Policy-critical: `deleted_at IS NULL` means a soft-deleted substrate
    // row is NOT considered live, so its vector is swept.
    // To preserve vectors for soft-deleted subjects, remove the
    // `deleted_at IS NULL` filter from both lines below (one-line change per
    // table).  The `memories` table referenced in ADR-044 §5 does not exist;
    // memory notes live in the `notes` table with kind = 'memory'.
    let live_subq = "SELECT id FROM entities WHERE deleted_at IS NULL \
                     UNION ALL \
                     SELECT id FROM notes    WHERE deleted_at IS NULL";

    let orphan_pred = format!(
        "subject_id NOT IN ({live}) AND {f}",
        live = live_subq,
        f = filter_pred,
    );

    // 1. Scanned: rows matching the caller's filters (before orphan check).
    let scan_sql = format!(
        "SELECT COUNT(*) FROM {t} WHERE {f}",
        t = table,
        f = filter_pred
    );
    let scanned: i64 = conn.query_row(
        &scan_sql,
        rusqlite::params![ns_json, kind_json, allow_json],
        |row| row.get(0),
    )?;

    // 2. Would-delete: orphaned rows among the scanned set.
    let count_sql = format!(
        "SELECT COUNT(*) FROM {t} WHERE {p}",
        t = table,
        p = orphan_pred,
    );
    let would_delete: i64 = conn.query_row(
        &count_sql,
        rusqlite::params![ns_json, kind_json, allow_json],
        |row| row.get(0),
    )?;

    let max_delete_hit = would_delete > max_delete;

    // 3. Delete — skipped in dry-run mode.
    //
    // `DELETE … LIMIT N` requires SQLITE_ENABLE_UPDATE_DELETE_LIMIT, which
    // rusqlite's bundled SQLite does not enable.  Portable alternative:
    // delete subject_ids returned by a capped SELECT subquery.  SQLite
    // materialises the inner SELECT before running the outer DELETE, so there
    // is no self-referential conflict.
    // Materialize the capped victim set first: the same `LIMIT` subquery
    // evaluated twice (once to log deletes, once to delete) has no ordering
    // guarantee, so logging and deleting must share one explicit id list.
    let deleted: i64 = if dry_run {
        0
    } else {
        let select_sql = format!(
            "SELECT subject_id FROM {t} WHERE {p} LIMIT ?4",
            t = table,
            p = orphan_pred,
        );
        let mut stmt = conn.prepare(&select_sql)?;
        let victim_ids: Vec<String> = stmt
            .query_map(
                rusqlite::params![ns_json, kind_json, allow_json, max_delete],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<_, _>>()?;
        drop(stmt);

        let mut total: i64 = 0;
        for chunk in victim_ids.chunks(400) {
            let placeholders: String = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let in_clause = format!("subject_id IN ({placeholders})");
            let params: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            log_vector_deletes(conn, table, &in_clause, &params)?;
            let del_sql = format!("DELETE FROM {t} WHERE {in_clause}", t = table);
            let mut del_stmt = conn.prepare(&del_sql)?;
            for (i, id_str) in chunk.iter().enumerate() {
                del_stmt.raw_bind_parameter(i + 1, id_str.as_str())?;
            }
            total += del_stmt.raw_execute()? as i64;
        }
        total
    };

    Ok(OrphanSweepResult {
        scanned: scanned as u64,
        would_delete: would_delete as u64,
        deleted: deleted as u64,
        max_delete_hit,
    })
}

#[async_trait]
impl VectorStore for SqliteVecStore {
    async fn insert(
        &self,
        subject_id: Uuid,
        kind: SubstrateKind,
        namespace: &str,
        field: &str,
        vectors: Vec<Vec<f32>>,
    ) -> Result<(), StorageError> {
        if vectors.len() != 1 {
            return Err(StorageError::Unsupported {
                capability: StorageCapability::Vectors,
                operation: "vec_insert".into(),
                message: "sqlite-vec supports exactly one vector per record".into(),
            });
        }
        let embedding = vectors.into_iter().next().expect("len checked");

        let table = self.table_name.clone();
        let dims = self.dimensions;
        let namespace = namespace.to_string();
        let field = field.to_string();
        let kind_str = kind.to_string();
        let embedding_model = self.embedding_model.clone();

        if embedding.len() == dims {
            if let Some(idx) = non_finite_index(&embedding) {
                return Err(non_finite_vector_error("vec_insert", idx, embedding[idx]));
            }
        }

        // Capture the failpoint Arc (if any) from the thread-local on the
        // calling thread before handing the closure to spawn_blocking.
        let failpoint_flag = current_failpoint();

        // ADR-067 Component A (Fork C slice 2): when the write queue is
        // enabled, route through the pool-wide WriterTask. DML-only
        // closure — atomicity is provided by `vec_upsert_atomic_dml`'s
        // named SAVEPOINT rather than `conn.unchecked_transaction()`,
        // which would attempt a nested `BEGIN` and fail under the
        // WriterTask's already-open transaction.
        if let Some(writer_task) = &self.writer_task {
            let table2 = table.clone();
            let namespace2 = namespace.clone();
            let field2 = field.clone();
            let kind_str2 = kind_str.clone();
            let embedding_model2 = embedding_model.clone();
            let embedding2 = embedding.clone();
            return writer_task
                .send(move |conn| {
                    vec_upsert_atomic_dml(
                        conn,
                        &table2,
                        dims,
                        subject_id,
                        &kind_str2,
                        &namespace2,
                        &field2,
                        &embedding_model2,
                        &embedding2,
                        "vec_insert_atomic",
                        failpoint_flag,
                    )
                    .map_err(|e| map_err(e, "vec_insert"))
                })
                .await;
        }

        // Flag-off (default) path: the closure owns its own transaction via
        // `conn.unchecked_transaction()`; the DELETE+INSERT body is the same
        // shared helper the WriterTask/batch paths use (#546), so this path
        // now also exercises the post-delete failpoint in tests.
        let origin = self.pool.origin();
        self.with_writer("vec_insert", move |conn| {
            // ADR-091 Plank 0: register the span before opening the transaction so
            // the handle (declared first) drops AFTER `tx` (declared second) —
            // locals drop in reverse declaration order, so `tx`'s own Drop (which
            // rolls back if uncommitted) runs while the registry entry is still
            // present.
            let _tx_handle = khive_storage::tx_registry::register_scoped(
                Some("vec_insert_tx".to_string()),
                origin,
            );
            let tx = conn.unchecked_transaction()?;

            replace_vector_row_dml(
                &tx,
                &table,
                dims,
                VectorRowRef {
                    subject_id,
                    namespace: &namespace,
                    kind: &kind_str,
                    field: &field,
                    embedding_model: &embedding_model,
                    embedding: &embedding,
                },
                failpoint_flag,
            )?;

            tx.commit()
        })
        .await
    }

    async fn insert_batch(
        &self,
        records: Vec<VectorRecord>,
    ) -> Result<BatchWriteSummary, StorageError> {
        let table = self.table_name.clone();
        let dims = self.dimensions;
        let attempted = records.len() as u64;
        let store_embedding_model = self.embedding_model.clone();

        // Capture the failpoint Arc (if any) from the thread-local on the
        // calling thread before handing the closure to spawn_blocking — both
        // the WriterTask path and the legacy path eventually run the closure
        // on a different thread than the one that reads the thread-local.
        let failpoint_flag = current_failpoint();

        // ADR-067 Component A: when the write queue is enabled, route
        // through the pool-wide WriterTask. DML-only closure (the per-record
        // `SAVEPOINT vec_batch_record` is preserved unchanged — only the
        // OUTER BEGIN IMMEDIATE/COMMIT is removed, since the WriterTask's
        // run loop owns the enclosing transaction).
        if let Some(writer_task) = &self.writer_task {
            let table2 = table.clone();
            let store_embedding_model2 = store_embedding_model.clone();
            return writer_task
                .send(move |conn| {
                    batch_insert_vectors_dml(
                        conn,
                        &table2,
                        dims,
                        &store_embedding_model2,
                        &records,
                        attempted,
                        failpoint_flag,
                    )
                    .map_err(|e| map_err(e, "vec_insert_batch"))
                })
                .await;
        }

        // Flag-off (default) path: byte-for-byte unchanged from pre-ADR-067
        // behavior — the closure owns its own BEGIN IMMEDIATE/COMMIT.
        let origin = self.pool.origin();
        self.with_writer("vec_insert_batch", move |conn| {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let _tx_handle = khive_storage::tx_registry::register_scoped(
                Some("vector_insert_batch".to_string()),
                origin,
            );

            let summary = batch_insert_vectors_dml(
                conn,
                &table,
                dims,
                &store_embedding_model,
                &records,
                attempted,
                failpoint_flag,
            )?;

            conn.execute_batch("COMMIT")?;

            Ok(summary)
        })
        .await
    }

    async fn update(
        &self,
        subject_id: Uuid,
        kind: SubstrateKind,
        namespace: &str,
        field: &str,
        vectors: Vec<Vec<f32>>,
    ) -> Result<(), StorageError> {
        if vectors.len() != 1 {
            return Err(StorageError::Unsupported {
                capability: StorageCapability::Vectors,
                operation: "vec_update".into(),
                message: "sqlite-vec supports exactly one vector per record".into(),
            });
        }
        let embedding = vectors.into_iter().next().expect("len checked");

        let table = self.table_name.clone();
        let dims = self.dimensions;
        let namespace = namespace.to_string();
        let field = field.to_string();
        let kind_str = kind.to_string();
        let embedding_model = self.embedding_model.clone();

        if embedding.len() == dims {
            if let Some(idx) = non_finite_index(&embedding) {
                return Err(non_finite_vector_error("vec_update", idx, embedding[idx]));
            }
        }

        // Capture the failpoint Arc (if any) from the thread-local on the
        // calling thread before handing the closure to spawn_blocking.
        let failpoint_flag = current_failpoint();

        // ADR-067 Component A (Fork C slice 2): when the write queue is
        // enabled, route through the pool-wide WriterTask. DML-only
        // closure — atomicity is provided by `vec_upsert_atomic_dml`'s
        // named SAVEPOINT rather than `conn.unchecked_transaction()`,
        // which would attempt a nested `BEGIN` and fail under the
        // WriterTask's already-open transaction.
        if let Some(writer_task) = &self.writer_task {
            let table2 = table.clone();
            let namespace2 = namespace.clone();
            let field2 = field.clone();
            let kind_str2 = kind_str.clone();
            let embedding_model2 = embedding_model.clone();
            let embedding2 = embedding.clone();
            return writer_task
                .send(move |conn| {
                    vec_upsert_atomic_dml(
                        conn,
                        &table2,
                        dims,
                        subject_id,
                        &kind_str2,
                        &namespace2,
                        &field2,
                        &embedding_model2,
                        &embedding2,
                        "vec_update_atomic",
                        failpoint_flag,
                    )
                    .map_err(|e| map_err(e, "vec_update"))
                })
                .await;
        }

        // Flag-off (default) path: the closure owns its own transaction via
        // `conn.unchecked_transaction()`; the DELETE+INSERT body is the same
        // shared helper the WriterTask/batch paths use (#546).
        let origin = self.pool.origin();
        self.with_writer("vec_update", move |conn| {
            // ADR-091 Plank 0: registered before the transaction is opened — see
            // the matching note in `insert()` above for the drop-order rationale.
            let _tx_handle = khive_storage::tx_registry::register_scoped(
                Some("vec_update_tx".to_string()),
                origin,
            );
            let tx = conn.unchecked_transaction()?;

            replace_vector_row_dml(
                &tx,
                &table,
                dims,
                VectorRowRef {
                    subject_id,
                    namespace: &namespace,
                    kind: &kind_str,
                    field: &field,
                    embedding_model: &embedding_model,
                    embedding: &embedding,
                },
                failpoint_flag,
            )?;

            tx.commit()
        })
        .await
    }

    async fn delete(&self, subject_id: Uuid) -> Result<bool, StorageError> {
        let statement = delete_vector_statement(&self.table_name, subject_id, &self.namespace);
        let table = self.table_name.clone();
        let namespace = self.namespace.clone();

        self.with_writer("vec_delete", move |conn| {
            conn.execute_batch("SAVEPOINT vec_delete_log")?;
            let result = (|| {
                log_vector_deletes(
                    conn,
                    &table,
                    "subject_id = ?1 AND namespace = ?2",
                    &[&subject_id.to_string(), &namespace],
                )?;
                let mut stmt = conn.prepare(&statement.sql)?;
                bind_params(&mut stmt, &statement.params)?;
                Ok(stmt.raw_execute()? > 0)
            })();
            match result {
                Ok(v) => {
                    conn.execute_batch("RELEASE SAVEPOINT vec_delete_log")?;
                    Ok(v)
                }
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK TO SAVEPOINT vec_delete_log");
                    let _ = conn.execute_batch("RELEASE SAVEPOINT vec_delete_log");
                    Err(e)
                }
            }
        })
        .await
    }

    async fn count(&self) -> Result<u64, StorageError> {
        let table = self.table_name.clone();
        let namespace = self.namespace.clone();

        self.with_reader("vec_count", move |conn| {
            let sql = format!("SELECT COUNT(*) FROM {} WHERE namespace = ?1", table);
            let count: i64 =
                conn.query_row(&sql, rusqlite::params![&namespace], |row| row.get(0))?;
            Ok(count as u64)
        })
        .await
    }

    async fn search(
        &self,
        request: VectorSearchRequest,
    ) -> Result<Vec<VectorSearchHit>, StorageError> {
        if request.filter.as_ref().is_some_and(|f| !f.is_empty()) {
            return Err(StorageError::Unsupported {
                capability: StorageCapability::Vectors,
                operation: "vec_search".into(),
                message: "use search_with_filter for filtered queries".into(),
            });
        }
        if request.query_vectors.len() != 1 {
            return Err(StorageError::Unsupported {
                capability: StorageCapability::Vectors,
                operation: "vec_search".into(),
                message: "sqlite-vec supports exactly one query vector per search".into(),
            });
        }
        let query_embedding = request.query_vectors[0].clone();

        let table = self.table_name.clone();
        let dims = self.dimensions;
        // Use request.namespace if present; fall back to self.namespace.
        let namespace = request
            .namespace
            .clone()
            .unwrap_or_else(|| self.namespace.clone());
        let kind_filter = request.kind.map(|k| k.to_string());
        // Use the request's embedding_model filter, or fall back to this store's model.
        let effective_model = request
            .embedding_model
            .clone()
            .unwrap_or_else(|| self.embedding_model.clone());

        if query_embedding.len() == dims {
            if let Some(idx) = non_finite_index(&query_embedding) {
                return Err(non_finite_vector_error(
                    "vec_search",
                    idx,
                    query_embedding[idx],
                ));
            }
        }

        self.with_reader("vec_search", move |conn| {
            if query_embedding.len() != dims {
                return Err(rusqlite::Error::InvalidParameterCount(
                    query_embedding.len(),
                    dims,
                ));
            }

            // Push namespace+embedding_model (and optionally kind) directly into
            // the MATCH predicate so sqlite-vec evaluates them before computing
            // global top-k, preventing cross-namespace recall starvation.
            let kind_clause = if kind_filter.is_some() {
                "AND kind = ?5"
            } else {
                ""
            };
            let sql = format!(
                "SELECT subject_id, distance \
                 FROM {t} \
                 WHERE embedding MATCH ?1 \
                   AND namespace = ?3 \
                   AND embedding_model = ?4 \
                   {kind_clause} \
                 ORDER BY distance \
                 LIMIT ?2",
                t = table,
                kind_clause = kind_clause
            );

            let query_blob = f32_slice_as_bytes(&query_embedding);
            let mut stmt = conn.prepare(&sql)?;

            // Collect rows into a Vec to avoid holding MappedRows (which is
            // parameterised on its closure type) across both branches.
            let raw_rows: Vec<rusqlite::Result<(String, f64)>> =
                if let Some(ref kind_str) = kind_filter {
                    stmt.query_map(
                        rusqlite::params![
                            query_blob,
                            request.top_k,
                            &namespace,
                            &effective_model,
                            kind_str
                        ],
                        |row| {
                            let id_str: String = row.get(0)?;
                            let distance: f64 = row.get(1)?;
                            Ok((id_str, distance))
                        },
                    )?
                    .collect()
                } else {
                    stmt.query_map(
                        rusqlite::params![query_blob, request.top_k, &namespace, &effective_model],
                        |row| {
                            let id_str: String = row.get(0)?;
                            let distance: f64 = row.get(1)?;
                            Ok((id_str, distance))
                        },
                    )?
                    .collect()
                };

            let mut hits = Vec::new();
            for (rank_idx, row) in raw_rows.into_iter().enumerate() {
                let (id_str, distance) = row?;
                let subject_id = Uuid::parse_str(&id_str).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;

                // sqlite-vec cosine distance: 0.0 = identical, 2.0 = opposite.
                // Convert to similarity in [0, 1]: score = 1.0 - (distance / 2.0)
                let similarity = 1.0 - (distance / 2.0);

                hits.push(VectorSearchHit {
                    subject_id,
                    score: DeterministicScore::from_f64(similarity),
                    rank: (rank_idx + 1) as u32,
                });
            }

            Ok(hits)
        })
        .await
    }

    async fn info(&self) -> Result<VectorStoreInfo, StorageError> {
        let count = self.count().await?;

        Ok(VectorStoreInfo {
            model_name: self.model_key.clone(),
            dimensions: self.dimensions,
            index_kind: VectorIndexKind::SqliteVec,
            entry_count: count,
            needs_rebuild: false,
            last_rebuild_at: None,
        })
    }

    async fn rebuild(&self, _scope: IndexRebuildScope) -> Result<VectorStoreInfo, StorageError> {
        // sqlite-vec uses brute-force search — no index to rebuild.
        self.info().await
    }

    async fn delete_subjects(&self, ids: &[Uuid]) -> Result<u64, StorageError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let table = self.table_name.clone();
        let id_strings: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
        let mut total_deleted: u64 = 0;

        // Batch in ≤400 IDs per statement to stay within SQLite's variable limit.
        for chunk in id_strings.chunks(400) {
            let placeholders: String = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!("DELETE FROM {table} WHERE subject_id IN ({placeholders})");
            let in_clause = format!("subject_id IN ({placeholders})");
            let chunk_owned = chunk.to_vec();
            let table_cl = table.clone();
            let table_sp = table.clone();
            let deleted = self
                .with_writer("vec_delete_subjects", move |conn| {
                    conn.execute_batch("SAVEPOINT vec_delete_subjects_log")?;
                    let result = (|| {
                        let params: Vec<&dyn rusqlite::ToSql> = chunk_owned
                            .iter()
                            .map(|s| s as &dyn rusqlite::ToSql)
                            .collect();
                        log_vector_deletes(conn, &table_sp, &in_clause, &params)?;
                        let mut stmt = conn.prepare(&sql)?;
                        for (i, id_str) in chunk_owned.iter().enumerate() {
                            stmt.raw_bind_parameter(i + 1, id_str.as_str())?;
                        }
                        stmt.raw_execute().map(|n| n as u64)
                    })();
                    match result {
                        Ok(n) => {
                            conn.execute_batch("RELEASE SAVEPOINT vec_delete_subjects_log")?;
                            Ok(n)
                        }
                        Err(e) => {
                            let _ =
                                conn.execute_batch("ROLLBACK TO SAVEPOINT vec_delete_subjects_log");
                            let _ = conn.execute_batch("RELEASE SAVEPOINT vec_delete_subjects_log");
                            Err(e)
                        }
                    }
                })
                .await
                .map_err(|e| {
                    tracing::warn!(error = %e, table = %table_cl, "delete_subjects chunk failed");
                    e
                })?;
            total_deleted += deleted;
        }
        Ok(total_deleted)
    }

    async fn batch_exists(
        &self,
        ids: &[Uuid],
        namespace: &str,
    ) -> Result<HashSet<Uuid>, StorageError> {
        if ids.is_empty() {
            return Ok(HashSet::new());
        }

        let table = self.table_name.clone();
        let namespace = namespace.to_string();
        let model = self.embedding_model.clone();
        let id_strings: Vec<String> = ids.iter().map(|id| id.to_string()).collect();

        self.with_reader("vec_batch_exists", move |conn| {
            let mut found = HashSet::new();

            for chunk in id_strings.chunks(400) {
                // ?1 = namespace, ?2 = embedding_model, ?3.. = subject IDs.
                let placeholders: String = (0..chunk.len())
                    .map(|i| format!("?{}", i + 3))
                    .collect::<Vec<_>>()
                    .join(", ");

                let sql = format!(
                    "SELECT subject_id FROM {} WHERE namespace = ?1 \
                     AND embedding_model = ?2 AND subject_id IN ({})",
                    table, placeholders
                );

                let mut stmt = conn.prepare(&sql)?;
                stmt.raw_bind_parameter(1, namespace.as_str())?;
                stmt.raw_bind_parameter(2, model.as_str())?;
                for (i, id_str) in chunk.iter().enumerate() {
                    stmt.raw_bind_parameter(i + 3, id_str.as_str())?;
                }

                let mut rows = stmt.raw_query();
                while let Some(row) = rows.next()? {
                    let id_str: String = row.get(0)?;
                    if let Ok(uuid) = Uuid::parse_str(&id_str) {
                        found.insert(uuid);
                    }
                }
            }

            Ok(found)
        })
        .await
    }

    async fn orphan_sweep(&self, config: &OrphanSweepConfig) -> StorageResult<OrphanSweepResult> {
        let table = self.table_name.clone();

        // Serialize filter lists as JSON arrays for json_each() usage inside SQL.
        // An empty list becomes None, which binds as NULL; the IS NULL guard then
        // short-circuits to true, passing all rows through (= no filtering).
        let ns_json: Option<String> = if config.namespaces.is_empty() {
            None
        } else {
            serde_json::to_string(&config.namespaces).ok()
        };

        let kind_json: Option<String> = if config.substrate_kinds.is_empty() {
            None
        } else {
            let strs: Vec<String> = config
                .substrate_kinds
                .iter()
                .map(|k| k.to_string())
                .collect();
            serde_json::to_string(&strs).ok()
        };

        // None = all rows eligible; Some(ids) = only those IDs may be swept.
        let allow_json: Option<String> = config.subject_id_allowlist.as_ref().map(|ids| {
            let strs: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
            serde_json::to_string(&strs).unwrap_or_default()
        });

        let max_delete = config.max_delete as i64;
        let dry_run = config.dry_run;

        // ADR-067 Amendment 1: when the write queue is enabled, route through
        // the pool-wide WriterTask. DML-only closure — `run_writer_task`'s
        // drain loop already owns the enclosing `BEGIN IMMEDIATE`/`COMMIT`/
        // `ROLLBACK` for this request, so the closure must not open or commit
        // its own transaction; issuing `Transaction::new_unchecked`'s `BEGIN
        // IMMEDIATE` here would violate SQLite's nested-transaction rule and
        // fail with `SQLITE_ERROR: cannot start a transaction within a
        // transaction` (ADR-067 lines 271-276).
        if let Some(writer_task) = &self.writer_task {
            let table2 = table.clone();
            let ns_json2 = ns_json.clone();
            let kind_json2 = kind_json.clone();
            let allow_json2 = allow_json.clone();
            return writer_task
                .send(move |conn| {
                    orphan_sweep_dml(
                        conn,
                        &table2,
                        ns_json2.as_deref(),
                        kind_json2.as_deref(),
                        allow_json2.as_deref(),
                        max_delete,
                        dry_run,
                    )
                    .map_err(|e| map_err(e, "orphan_sweep"))
                })
                .await;
        }

        // Flag-off (default) path: byte-for-byte unchanged from pre-ADR-067
        // behavior — the closure owns its own transaction via
        // `Transaction::new_unchecked`.
        let origin = self.pool.origin();
        self.with_writer_unmanaged("orphan_sweep", move |conn| {
            // `Transaction::new_unchecked` issues `BEGIN IMMEDIATE` and RAII-manages
            // rollback via its Drop impl: it checks `conn.is_autocommit()` and issues
            // ROLLBACK when the connection still has an open transaction — covering both
            // early-`?` errors AND a COMMIT that fails with SQLITE_BUSY (BUSY leaves
            // the transaction open, so autocommit is false, and Drop rolls back).
            // The hand-rolled guard used previously set `done = true` before COMMIT,
            // which would have skipped the Drop-ROLLBACK on a BUSY COMMIT and re-poisoned
            // the pool.  Using the native primitive avoids that class of bug entirely.
            //
            // `with_writer_unmanaged` serialises all callers through the pool mutex — at
            // most one writer closure executes on this connection at a time, so no nested
            // transactions can exist when this line runs.
            //
            // ADR-091 Plank 0: registered before the transaction is opened — see the
            // matching note in `insert()` for the drop-order rationale (the handle,
            // declared first, drops after `tx`'s own Drop/rollback runs).
            let _tx_handle = khive_storage::tx_registry::register_scoped(
                Some("vec_orphan_sweep".to_string()),
                origin,
            );
            let tx = rusqlite::Transaction::new_unchecked(
                conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;

            let result = orphan_sweep_dml(
                conn,
                &table,
                ns_json.as_deref(),
                kind_json.as_deref(),
                allow_json.as_deref(),
                max_delete,
                dry_run,
            )?;

            tx.commit()?;

            Ok(result)
        })
        .await
    }

    fn capabilities(&self) -> &'static VectorStoreCapabilities {
        static SQLITE_VEC_CAPABILITIES: OnceLock<VectorStoreCapabilities> = OnceLock::new();
        SQLITE_VEC_CAPABILITIES.get_or_init(|| VectorStoreCapabilities {
            supports_filter: false,
            supports_batch_search: false,
            supports_quantization: false,
            supports_update: false,
            supports_orphan_sweep: true,
            // sqlite-vec uses subject_id as PRIMARY KEY — only one vector per
            // subject per namespace is stored. Callers must use a single canonical
            // field (e.g. "content") and are not permitted to store both
            // "entity.title" and "entity.body" as separate vectors in one table.
            supports_multi_field: false,
            // sqlite-vec 0.1.9 rejects dimensions > SQLITE_VEC_VEC0_MAX_DIMENSIONS (8192).
            // Reporting 8192 lets callers know that 4097–8192 dimensional models are
            // supported. The previous value of 4096 was the K_MAX (neighbors per query)
            // constant, not the dimension limit.
            max_dimensions: Some(8192),
            index_kinds: vec![VectorIndexKind::SqliteVec],
        })
    }
}

impl SqliteVecStore {
    /// Score a fixed set of candidate IDs against a query embedding.
    ///
    /// Unlike `search`, this does not use the MATCH index — it computes cosine
    /// distance directly for the supplied IDs only. Results are returned sorted
    /// by descending score.
    pub async fn score_candidates(
        &self,
        query_embedding: &[f32],
        candidate_ids: &[Uuid],
    ) -> Result<Vec<VectorSearchHit>, StorageError> {
        if candidate_ids.is_empty() || query_embedding.is_empty() {
            return Ok(Vec::new());
        }

        let dims = self.dimensions;
        if query_embedding.len() != dims {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Vectors,
                operation: "score_candidates".into(),
                message: format!(
                    "query has {} dims, expected {}",
                    query_embedding.len(),
                    dims
                ),
            });
        }

        if let Some(idx) = non_finite_index(query_embedding) {
            return Err(non_finite_vector_error(
                "score_candidates",
                idx,
                query_embedding[idx],
            ));
        }

        let table = self.table_name.clone();
        let namespace = self.namespace.clone();
        let embedding_model = self.embedding_model.clone();
        let query_vec = query_embedding.to_vec();
        let ids: Vec<String> = candidate_ids.iter().map(|id| id.to_string()).collect();

        self.with_reader("score_candidates", move |conn| {
            let mut all_hits: Vec<VectorSearchHit> = Vec::new();
            let query_blob = f32_slice_as_bytes(&query_vec);

            for chunk in ids.chunks(399) {
                let placeholders: String = chunk
                    .iter()
                    .enumerate()
                    .map(|(i, _)| format!("?{}", i + 4))
                    .collect::<Vec<_>>()
                    .join(", ");

                let sql = format!(
                    "SELECT e.subject_id, vec_distance_cosine(e.embedding, ?1) as distance \
                     FROM {} e \
                     WHERE e.namespace = ?2 AND e.embedding_model = ?3 \
                       AND e.subject_id IN ({})",
                    table, placeholders
                );

                let mut stmt = conn.prepare(&sql)?;
                stmt.raw_bind_parameter(1, query_blob)?;
                stmt.raw_bind_parameter(2, namespace.as_str())?;
                stmt.raw_bind_parameter(3, embedding_model.as_str())?;
                for (i, id_str) in chunk.iter().enumerate() {
                    stmt.raw_bind_parameter(i + 4, id_str.as_str())?;
                }

                let mut rows = stmt.raw_query();
                while let Some(row) = rows.next()? {
                    let id_str: String = row.get(0)?;
                    let distance: f64 = row.get(1)?;

                    let subject_id = Uuid::parse_str(&id_str).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?;

                    let similarity = 1.0 - (distance / 2.0);
                    all_hits.push(VectorSearchHit {
                        subject_id,
                        score: DeterministicScore::from_f64(similarity),
                        rank: 0,
                    });
                }
            }

            all_hits.sort_by_key(|hit| std::cmp::Reverse(hit.score));
            for (i, hit) in all_hits.iter_mut().enumerate() {
                hit.rank = (i + 1) as u32;
            }

            Ok(all_hits)
        })
        .await
    }
}

#[cfg(all(test, feature = "vectors"))]
mod batch_exists_tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use khive_types::SubstrateKind;
    use uuid::Uuid;

    use super::*;

    fn make_vec_pool() -> Arc<crate::pool::ConnectionPool> {
        use crate::pool::{ConnectionPool, PoolConfig};
        crate::extension::ensure_extensions_loaded();
        let config = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        Arc::new(ConnectionPool::new(config).expect("in-memory pool"))
    }

    fn create_vec_table(pool: &Arc<crate::pool::ConnectionPool>, model_key: &str, dims: usize) {
        let writer = pool.try_writer().expect("pool writer");
        let ddl = format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS vec_{} USING vec0(\
             subject_id TEXT PRIMARY KEY, \
             namespace TEXT NOT NULL, \
             kind TEXT NOT NULL, \
             field TEXT NOT NULL, \
             embedding_model TEXT NOT NULL, \
             embedding float[{}] distance_metric=cosine)",
            model_key, dims
        );
        writer.conn().execute_batch(&ddl).expect("create vec table");
        writer
            .conn()
            .execute_batch(crate::migrations::ANN_WRITE_LOG_DDL)
            .expect("create ann_write_log");
    }

    /// Valid (underscored) model key: batch_exists returns the exact set of IDs
    /// that have embeddings and excludes IDs that were never inserted.
    #[tokio::test]
    async fn batch_exists_returns_correct_set_for_underscored_model_key() {
        let pool = make_vec_pool();
        let model_key = "all_minilm_l6_v2";
        let dims = 4;
        let ns = "ns:test";

        create_vec_table(&pool, model_key, dims);

        let store = SqliteVecStore::new(
            pool,
            false,
            model_key.to_string(),
            model_key.to_string(),
            dims,
            ns.to_string(),
        )
        .expect("SqliteVecStore::new");

        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let id_absent = Uuid::new_v4();

        store
            .insert(
                id1,
                SubstrateKind::Entity,
                ns,
                "body",
                vec![vec![0.1, 0.2, 0.3, 0.4]],
            )
            .await
            .expect("insert id1");
        store
            .insert(
                id2,
                SubstrateKind::Entity,
                ns,
                "body",
                vec![vec![0.5, 0.6, 0.7, 0.8]],
            )
            .await
            .expect("insert id2");

        let exists = store
            .batch_exists(&[id1, id2, id_absent], ns)
            .await
            .expect("batch_exists");

        assert!(exists.contains(&id1), "id1 must be found");
        assert!(exists.contains(&id2), "id2 must be found");
        assert!(
            !exists.contains(&id_absent),
            "absent id must not be returned"
        );
        assert_eq!(exists.len(), 2);
    }

    /// Empty input must return an empty set without hitting the DB.
    #[tokio::test]
    async fn batch_exists_empty_ids_returns_empty_set() {
        let pool = make_vec_pool();
        let model_key = "empty_test_model";
        create_vec_table(&pool, model_key, 4);

        let store = SqliteVecStore::new(
            pool,
            false,
            model_key.to_string(),
            model_key.to_string(),
            4,
            "ns:test".to_string(),
        )
        .expect("SqliteVecStore::new");

        let exists: HashSet<Uuid> = store
            .batch_exists(&[], "ns:test")
            .await
            .expect("batch_exists");
        assert!(exists.is_empty());
    }

    /// A nearer vector in namespace A must not starve the top-k result in namespace B.
    ///
    /// Regression for the cross-namespace recall starvation path: sqlite-vec must
    /// evaluate the namespace predicate before computing global top-k, not after.
    #[tokio::test]
    async fn vector_search_namespace_predicate_prevents_recall_starvation() {
        let pool = make_vec_pool();
        let model_key = "knn_namespace_scope";
        let dims = 4;
        create_vec_table(&pool, model_key, dims);

        let store = SqliteVecStore::new(
            pool,
            false,
            model_key.to_string(),
            model_key.to_string(),
            dims,
            "ns:b".to_string(),
        )
        .expect("SqliteVecStore::new");

        let distractor_a = Uuid::new_v4();
        let victim_b = Uuid::new_v4();

        // Insert a nearer vector in namespace A (distractor).
        store
            .insert(
                distractor_a,
                SubstrateKind::Entity,
                "ns:a",
                "body",
                vec![vec![1.0, 0.0, 0.0, 0.0]],
            )
            .await
            .expect("insert nearer cross-namespace vector");

        // Insert a slightly farther vector in namespace B (victim).
        store
            .insert(
                victim_b,
                SubstrateKind::Entity,
                "ns:b",
                "body",
                vec![vec![0.8, 0.2, 0.0, 0.0]],
            )
            .await
            .expect("insert in-namespace vector");

        // top_k=1 search in ns:b must return victim_b, not the nearer distractor_a.
        let hits = store
            .search(VectorSearchRequest {
                query_vectors: vec![vec![1.0, 0.0, 0.0, 0.0]],
                top_k: 1,
                namespace: Some("ns:b".to_string()),
                kind: Some(SubstrateKind::Entity),
                embedding_model: None,
                filter: None,
                backend_hints: None,
            })
            .await
            .expect("search");

        assert_eq!(
            hits.len(),
            1,
            "namespace B must not be starved by namespace A"
        );
        assert_eq!(
            hits[0].subject_id, victim_b,
            "top-1 in ns:b must be victim_b, not cross-namespace distractor_a"
        );
    }

    /// Hyphenated model_key must be rejected at SqliteVecStore::new(), preventing
    /// any table-name divergence between the store and a hand-rolled sanitizer.
    #[test]
    fn hyphenated_model_key_is_rejected_at_construction() {
        use crate::pool::{ConnectionPool, PoolConfig};
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: None,
                ..PoolConfig::default()
            })
            .expect("pool"),
        );

        let result = SqliteVecStore::new(
            pool,
            false,
            "all-minilm-l6-v2".to_string(),
            "all-minilm-l6-v2".to_string(),
            4,
            "ns:test".to_string(),
        );

        assert!(
            result.is_err(),
            "hyphenated model_key 'all-minilm-l6-v2' must be rejected; \
             the store's table_name would differ from what a hand-rolled sanitizer produces"
        );
    }
}

/// Tests for `first_error` surfacing in `insert_batch`.
///
/// These tests use only the pre-SAVEPOINT validation path (wrong vector count
/// or wrong dimensions) so they do not need the `vectors` feature; no vec0
/// virtual table is accessed.
#[cfg(test)]
mod first_error_tests {
    use super::*;
    use khive_storage::types::VectorRecord;
    use khive_storage::VectorStore;
    use khive_types::SubstrateKind;
    use uuid::Uuid;

    fn make_pool() -> Arc<crate::pool::ConnectionPool> {
        use crate::pool::{ConnectionPool, PoolConfig};
        let config = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        Arc::new(ConnectionPool::new(config).expect("in-memory pool"))
    }

    /// insert_batch must populate `first_error` when records fail the dimension
    /// validation check.
    ///
    /// Both records have the wrong number of dimensions, so both hit the
    /// `embedding.len() != dims` guard before any SAVEPOINT or vec0 operation.
    /// The outer transaction still commits (best-effort batch semantics).
    ///
    /// Regression: before the fix, `first_error` was always `String::new()` even
    /// when `failed > 0`.  This test is RED against the unfixed code and GREEN
    /// after the fix.
    #[tokio::test]
    async fn insert_batch_first_error_populated_on_dimension_mismatch() {
        let dims = 4usize;
        let store = SqliteVecStore::new(
            make_pool(),
            false,
            "first_err_vec".into(),
            "first_err_vec".into(),
            dims,
            "ns:test".into(),
        )
        .expect("SqliteVecStore::new");

        // Both records have wrong dimensions, so they fail the pre-SAVEPOINT
        // validation and never touch the vec0 virtual table.
        let summary = store
            .insert_batch(vec![
                VectorRecord {
                    subject_id: Uuid::new_v4(),
                    kind: SubstrateKind::Entity,
                    namespace: "ns:test".to_string(),
                    field: "body".to_string(),
                    embedding_model: None,
                    vectors: vec![vec![0.0f32; dims + 1]],
                    updated_at: chrono::Utc::now(),
                },
                VectorRecord {
                    subject_id: Uuid::new_v4(),
                    kind: SubstrateKind::Entity,
                    namespace: "ns:test".to_string(),
                    field: "body".to_string(),
                    embedding_model: None,
                    vectors: vec![vec![0.0f32; dims + 2]],
                    updated_at: chrono::Utc::now(),
                },
            ])
            .await
            .expect("insert_batch must return Ok (best-effort semantics)");

        assert_eq!(summary.attempted, 2);
        assert_eq!(
            summary.failed, 2,
            "both wrong-dims records must be counted as failed"
        );
        assert_eq!(summary.affected, 0);
        assert!(
            !summary.first_error.is_empty(),
            "first_error must be populated when failed > 0; \
             got empty string; the validation error is silently swallowed"
        );
    }
}

#[cfg(test)]
mod capabilities_tests {
    use super::*;

    fn make_pool() -> Arc<crate::pool::ConnectionPool> {
        use crate::pool::{ConnectionPool, PoolConfig};
        let config = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        Arc::new(ConnectionPool::new(config).expect("in-memory pool"))
    }

    #[test]
    fn sqlite_vec_store_capabilities_are_correct() {
        let store = SqliteVecStore::new(
            make_pool(),
            /*is_file_backed=*/ false,
            "test_model".into(),
            "test_model".into(),
            /*dimensions=*/ 4,
            "ns:test".into(),
        )
        .expect("SqliteVecStore::new");

        let caps = store.capabilities();

        assert!(
            !caps.supports_filter,
            "sqlite-vec does not support filter pushdown"
        );
        assert!(
            !caps.supports_batch_search,
            "sqlite-vec does not support native batch search"
        );
        assert!(
            !caps.supports_quantization,
            "sqlite-vec does not support quantization"
        );
        assert!(
            !caps.supports_update,
            "sqlite-vec does not support in-place update"
        );
        assert!(
            caps.supports_orphan_sweep,
            "SqliteVecStore must advertise supports_orphan_sweep = true"
        );
        // sqlite-vec 0.1.9: SQLITE_VEC_VEC0_MAX_DIMENSIONS = 8192.
        assert_eq!(caps.max_dimensions, Some(8192));
        assert_eq!(
            caps.index_kinds,
            vec![VectorIndexKind::SqliteVec],
            "index_kinds should be [SqliteVec]"
        );
    }

    /// Regression: max_dimensions must equal the sqlite-vec hard limit (8192),
    /// not the K_MAX constant (4096). A caller with 5000-dim embeddings must not
    /// be falsely told the backend is incapable.
    #[test]
    fn max_dimensions_reflects_sqlite_vec_hard_limit_not_k_max() {
        let store = SqliteVecStore::new(
            make_pool(),
            false,
            "test_dim_limit".into(),
            "test_dim_limit".into(),
            /*dimensions=*/ 4,
            "ns:test".into(),
        )
        .expect("SqliteVecStore::new");

        let caps = store.capabilities();

        // SQLITE_VEC_VEC0_MAX_DIMENSIONS = 8192 (sqlite-vec.c:3488).
        // The previous incorrect value 4096 was SQLITE_VEC_VEC0_K_MAX (max neighbours),
        // which would falsely reject 4097–8192 dimensional models.
        let max = caps
            .max_dimensions
            .expect("SqliteVecStore must declare a finite dimension limit");
        assert!(
            max >= 8192,
            "max_dimensions ({max}) must be at least 8192 — the sqlite-vec hard limit"
        );
    }

    /// Capabilities struct is returned by &'static reference; calling twice must
    /// return the same value (OnceLock semantics, no allocation on repeat calls).
    #[test]
    fn capabilities_is_idempotent() {
        let store = SqliteVecStore::new(
            make_pool(),
            false,
            "test_idempotent".into(),
            "test_idempotent".into(),
            4,
            "ns:test".into(),
        )
        .expect("SqliteVecStore::new");

        let caps1 = store.capabilities();
        let caps2 = store.capabilities();
        assert_eq!(
            caps1 as *const _, caps2 as *const _,
            "capabilities() must return the same static reference each call"
        );
    }
}

#[cfg(all(test, feature = "vectors"))]
mod atomic_replace_tests {
    use std::sync::Arc;

    use khive_storage::types::VectorRecord;
    use khive_storage::VectorStore;
    use khive_types::SubstrateKind;
    use uuid::Uuid;

    use super::*;

    fn make_vec_pool() -> Arc<crate::pool::ConnectionPool> {
        use crate::pool::{ConnectionPool, PoolConfig};
        crate::extension::ensure_extensions_loaded();
        let config = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        Arc::new(ConnectionPool::new(config).expect("in-memory pool"))
    }

    fn create_vec_table(pool: &Arc<crate::pool::ConnectionPool>, model_key: &str, dims: usize) {
        let writer = pool.try_writer().expect("pool writer");
        let ddl = format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS vec_{} USING vec0(\
             subject_id TEXT PRIMARY KEY, \
             namespace TEXT NOT NULL, \
             kind TEXT NOT NULL, \
             field TEXT NOT NULL, \
             embedding_model TEXT NOT NULL, \
             embedding float[{}] distance_metric=cosine)",
            model_key, dims
        );
        writer.conn().execute_batch(&ddl).expect("create vec table");
        writer
            .conn()
            .execute_batch(crate::migrations::ANN_WRITE_LOG_DDL)
            .expect("create ann_write_log");
    }

    /// insert_batch: a record with wrong dimensions fails its INSERT but must not
    /// lose the previously stored vector (no-worse-than-stale guarantee for batch).
    ///
    /// Setup: insert a good vector for `id_existing` via the single-record path.
    /// Then call insert_batch with two records: `id_existing` with wrong dimensions
    /// (forced failure), and `id_new` with correct dimensions.
    /// Expected: `id_existing`'s old vector survives; `id_new` is inserted;
    /// BatchWriteSummary reflects 1 affected / 1 failed.
    #[tokio::test]
    async fn insert_batch_failed_record_preserves_prior_vector() {
        let pool = make_vec_pool();
        let model_key = "atomic_batch_test";
        let dims = 4;
        let ns = "ns:atomic";

        create_vec_table(&pool, model_key, dims);

        let store = SqliteVecStore::new(
            Arc::clone(&pool),
            false,
            model_key.to_string(),
            model_key.to_string(),
            dims,
            ns.to_string(),
        )
        .expect("SqliteVecStore::new");

        let id_existing = Uuid::new_v4();
        let id_new = Uuid::new_v4();
        let original_vec = vec![0.1f32, 0.2, 0.3, 0.4];

        store
            .insert(
                id_existing,
                SubstrateKind::Entity,
                ns,
                "body",
                vec![original_vec.clone()],
            )
            .await
            .expect("initial insert");

        let summary = store
            .insert_batch(vec![
                VectorRecord {
                    subject_id: id_existing,
                    kind: SubstrateKind::Entity,
                    namespace: ns.to_string(),
                    field: "body".to_string(),
                    embedding_model: None,
                    vectors: vec![vec![9.9f32; dims + 1]],
                    updated_at: chrono::Utc::now(),
                },
                VectorRecord {
                    subject_id: id_new,
                    kind: SubstrateKind::Entity,
                    namespace: ns.to_string(),
                    field: "body".to_string(),
                    embedding_model: None,
                    vectors: vec![vec![0.5f32, 0.6, 0.7, 0.8]],
                    updated_at: chrono::Utc::now(),
                },
            ])
            .await
            .expect("insert_batch");

        assert_eq!(summary.attempted, 2);
        assert_eq!(summary.affected, 1, "only id_new should succeed");
        assert_eq!(summary.failed, 1, "id_existing with wrong dims must fail");

        let existing_still_present = store
            .batch_exists(&[id_existing], ns)
            .await
            .expect("batch_exists");
        assert!(
            existing_still_present.contains(&id_existing),
            "prior vector for id_existing must survive a failed batch replace"
        );

        let new_present = store
            .batch_exists(&[id_new], ns)
            .await
            .expect("batch_exists for id_new");
        assert!(
            new_present.contains(&id_new),
            "id_new with valid dims must be inserted"
        );
    }

    /// update: a vector with wrong dimensions must fail without deleting the prior
    /// vector (no-worse-than-stale guarantee for the update override).
    #[tokio::test]
    async fn update_failed_preserves_prior_vector() {
        let pool = make_vec_pool();
        let model_key = "atomic_update_test";
        let dims = 4;
        let ns = "ns:atomic_upd";

        create_vec_table(&pool, model_key, dims);

        let store = SqliteVecStore::new(
            Arc::clone(&pool),
            false,
            model_key.to_string(),
            model_key.to_string(),
            dims,
            ns.to_string(),
        )
        .expect("SqliteVecStore::new");

        let id = Uuid::new_v4();

        store
            .insert(
                id,
                SubstrateKind::Entity,
                ns,
                "body",
                vec![vec![0.1f32, 0.2, 0.3, 0.4]],
            )
            .await
            .expect("initial insert");

        let result = store
            .update(
                id,
                SubstrateKind::Entity,
                ns,
                "body",
                vec![vec![9.9f32; dims + 1]],
            )
            .await;

        assert!(result.is_err(), "update with wrong dims must fail");

        let still_present = store
            .batch_exists(&[id], ns)
            .await
            .expect("batch_exists after failed update");
        assert!(
            still_present.contains(&id),
            "prior vector must survive a failed update"
        );
    }

    /// insert_batch: SAVEPOINT/ROLLBACK path — INSERT failure inside the
    /// savepoint via a PK conflict. See
    /// crates/khive-db/docs/api/vectors.md#insert_batch_savepoint_rollback_on_pk_conflict_preserves_stale
    #[tokio::test]
    async fn insert_batch_savepoint_rollback_on_pk_conflict_preserves_stale() {
        let pool = make_vec_pool();
        let model_key = "atomic_pk_batch";
        let dims = 4;
        let ns_a = "ns:pk_a";
        let ns_b = "ns:pk_b";

        create_vec_table(&pool, model_key, dims);

        let store = SqliteVecStore::new(
            Arc::clone(&pool),
            false,
            model_key.to_string(),
            model_key.to_string(),
            dims,
            ns_a.to_string(),
        )
        .expect("SqliteVecStore::new");

        let id_x = Uuid::new_v4();
        let stale_vec = vec![0.1f32, 0.2, 0.3, 0.4];

        // Store stale row in ns:a — this occupies id_X in the vec0 PK.
        store
            .insert(
                id_x,
                SubstrateKind::Entity,
                ns_a,
                "body",
                vec![stale_vec.clone()],
            )
            .await
            .expect("stale insert");

        // Batch: one record for (id_X, ns:b) — correct dims, all finite.
        // DELETE WHERE ns=ns:b finds nothing.  INSERT hits PK constraint.
        // Code path: SAVEPOINT → DELETE(noop) → INSERT(PK fail) →
        //            ROLLBACK TO SAVEPOINT → RELEASE → outer COMMIT.
        let summary = store
            .insert_batch(vec![VectorRecord {
                subject_id: id_x,
                kind: SubstrateKind::Entity,
                namespace: ns_b.to_string(),
                field: "body".to_string(),
                embedding_model: None,
                vectors: vec![vec![0.5f32, 0.6, 0.7, 0.8]],
                updated_at: chrono::Utc::now(),
            }])
            .await
            .expect("insert_batch must complete (outer tx must commit)");

        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.affected, 0, "PK conflict must count as failed");
        assert_eq!(
            summary.failed, 1,
            "failed counter must increment after ROLLBACK TO SAVEPOINT"
        );

        // Stale row must survive — no partial state must have leaked.
        let post = store
            .batch_exists(&[id_x], ns_a)
            .await
            .expect("batch_exists ns:a");
        assert!(
            post.contains(&id_x),
            "stale row in ns:a must survive after SAVEPOINT + INSERT failure"
        );

        // Verify embedding bytes via self-similarity — any shadow-table corruption
        // would produce a score below 1.0.
        let hits = store
            .search(VectorSearchRequest {
                query_vectors: vec![stale_vec.clone()],
                top_k: 1,
                namespace: Some(ns_a.to_string()),
                kind: Some(SubstrateKind::Entity),
                embedding_model: None,
                filter: None,
                backend_hints: None,
            })
            .await
            .expect("search ns:a after batch");

        assert_eq!(hits.len(), 1, "stale vector must be searchable");
        assert_eq!(hits[0].subject_id, id_x);
        let sim = hits[0].score.to_f64();
        assert!(
            sim > 0.999,
            "cosine similarity of stale_vec to itself must be ~1.0 (got {sim:.6}); \
             a lower value means the SAVEPOINT/ROLLBACK left partial writes visible"
        );
    }

    /// insert_batch: a rolled-back first record must not corrupt a
    /// successful second record. See
    /// crates/khive-db/docs/api/vectors.md#insert_batch_rollback_does_not_corrupt_subsequent_record
    #[tokio::test]
    async fn insert_batch_rollback_does_not_corrupt_subsequent_record() {
        let pool = make_vec_pool();
        let model_key = "atomic_sib_batch";
        let dims = 4;
        let ns_a = "ns:sib_a";
        let ns_b = "ns:sib_b";

        create_vec_table(&pool, model_key, dims);

        let store = SqliteVecStore::new(
            Arc::clone(&pool),
            false,
            model_key.to_string(),
            model_key.to_string(),
            dims,
            ns_a.to_string(),
        )
        .expect("SqliteVecStore::new");

        let id_x = Uuid::new_v4();
        let stale_vec = vec![0.1f32, 0.2, 0.3, 0.4];
        let new_vec = vec![0.9f32, 0.1, 0.1, 0.1];

        // Stale row occupies id_X in ns:a.
        store
            .insert(
                id_x,
                SubstrateKind::Entity,
                ns_a,
                "body",
                vec![stale_vec.clone()],
            )
            .await
            .expect("stale insert");

        // Record A (ns:b) fails — PK conflict; Record B (ns:a) succeeds — replaces stale.
        let summary = store
            .insert_batch(vec![
                VectorRecord {
                    subject_id: id_x,
                    kind: SubstrateKind::Entity,
                    namespace: ns_b.to_string(),
                    field: "body".to_string(),
                    embedding_model: None,
                    vectors: vec![vec![0.5f32, 0.6, 0.7, 0.8]],
                    updated_at: chrono::Utc::now(),
                },
                VectorRecord {
                    subject_id: id_x,
                    kind: SubstrateKind::Entity,
                    namespace: ns_a.to_string(),
                    field: "body".to_string(),
                    embedding_model: None,
                    vectors: vec![new_vec.clone()],
                    updated_at: chrono::Utc::now(),
                },
            ])
            .await
            .expect("insert_batch");

        assert_eq!(summary.attempted, 2);
        // Record A (ns:b) hits the PK constraint → failed.
        // Record B (ns:a) DELETEs the stale (freeing PK) then INSERTs → affected.
        assert_eq!(summary.affected, 1, "Record B must succeed");
        assert_eq!(summary.failed, 1, "Record A must fail (PK conflict)");

        // Record B's new_vec must be in the DB with correct embedding bytes.
        let hits = store
            .search(VectorSearchRequest {
                query_vectors: vec![new_vec.clone()],
                top_k: 1,
                namespace: Some(ns_a.to_string()),
                kind: Some(SubstrateKind::Entity),
                embedding_model: None,
                filter: None,
                backend_hints: None,
            })
            .await
            .expect("search after batch");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].subject_id, id_x);
        let sim = hits[0].score.to_f64();
        assert!(
            sim > 0.999,
            "new_vec similarity to itself must be ~1.0 (got {sim:.6}); \
             Record A's ROLLBACK must not corrupt Record B's write"
        );
    }

    /// update: PK-conflict INSERT inside `unchecked_transaction` rolls back
    /// and preserves the stale row. See
    /// crates/khive-db/docs/api/vectors.md#update_pk_conflict_rolls_back_transaction_preserves_stale
    #[tokio::test]
    async fn update_pk_conflict_rolls_back_transaction_preserves_stale() {
        let pool = make_vec_pool();
        let model_key = "atomic_upd_pk";
        let dims = 4;
        let ns_a = "ns:upk_a";
        let ns_b = "ns:upk_b";

        create_vec_table(&pool, model_key, dims);

        let store = SqliteVecStore::new(
            Arc::clone(&pool),
            false,
            model_key.to_string(),
            model_key.to_string(),
            dims,
            ns_a.to_string(),
        )
        .expect("store");

        let id_x = Uuid::new_v4();
        let stale_vec = vec![0.1f32, 0.2, 0.3, 0.4];

        // Store stale row in ns:a.
        store
            .insert(
                id_x,
                SubstrateKind::Entity,
                ns_a,
                "body",
                vec![stale_vec.clone()],
            )
            .await
            .expect("stale insert");

        // update() with ns:b — correct dims, finite values, but different namespace.
        // DELETE WHERE ns=ns:b finds nothing; INSERT id_X hits PK → transaction rolls back.
        let result = store
            .update(
                id_x,
                SubstrateKind::Entity,
                ns_b,
                "body",
                vec![vec![0.5f32, 0.6, 0.7, 0.8]],
            )
            .await;

        assert!(
            result.is_err(),
            "update must fail when INSERT hits the vec0 PK constraint"
        );

        // Stale row in ns:a must be intact.
        let post = store
            .batch_exists(&[id_x], ns_a)
            .await
            .expect("batch_exists after failed update");
        assert!(
            post.contains(&id_x),
            "stale row in ns:a must survive after update transaction rollback"
        );

        // Self-similarity check proves the embedding bytes are unchanged.
        let hits = store
            .search(VectorSearchRequest {
                query_vectors: vec![stale_vec.clone()],
                top_k: 1,
                namespace: Some(ns_a.to_string()),
                kind: Some(SubstrateKind::Entity),
                embedding_model: None,
                filter: None,
                backend_hints: None,
            })
            .await
            .expect("search after failed update");

        assert_eq!(hits.len(), 1, "stale vector must be searchable");
        assert_eq!(hits[0].subject_id, id_x);
        let sim = hits[0].score.to_f64();
        assert!(
            sim > 0.999,
            "cosine similarity of stale_vec to itself must be ~1.0 (got {sim:.6}); \
             transaction rollback must leave embedding bytes unchanged"
        );
    }

    // True ROLLBACK TO SAVEPOINT sentinels (failpoint-driven) — see
    // crates/khive-db/docs/api/vectors.md#true-rollback-to-savepoint-sentinels-failpoint-driven

    /// SENTINEL — insert_batch: stale row is restored when DELETE succeeds
    /// but INSERT is forced to fail via the cfg(test) failpoint. See
    /// crates/khive-db/docs/api/vectors.md#insert_batch_rollback_restores_deleted_stale_after_post_delete_insert_failure
    #[tokio::test]
    async fn insert_batch_rollback_restores_deleted_stale_after_post_delete_insert_failure() {
        let pool = make_vec_pool();
        let model_key = "sentinel_batch_rb";
        let dims = 4;
        let ns = "ns:sentinel_batch";

        create_vec_table(&pool, model_key, dims);

        let store = SqliteVecStore::new(
            Arc::clone(&pool),
            false,
            model_key.to_string(),
            model_key.to_string(),
            dims,
            ns.to_string(),
        )
        .expect("SqliteVecStore::new");

        let id_x = Uuid::new_v4();
        let vec1 = vec![0.1f32, 0.2, 0.3, 0.4];
        let vec2 = vec![0.9f32, 0.0, 0.0, 0.0];

        // Insert the stale row that must survive.
        store
            .insert(id_x, SubstrateKind::Entity, ns, "body", vec![vec1.clone()])
            .await
            .expect("stale insert");

        // Arm the failpoint under an RAII guard so it always clears on exit.
        // The guard is dropped AFTER the batch call returns, but `take()` is
        // one-shot — it clears the flag the moment the failpoint fires.
        let _guard = failpoint::FailpointGuard::new();

        // Same namespace, correct dims, finite — DELETE will run, then failpoint fires.
        let summary = store
            .insert_batch(vec![VectorRecord {
                subject_id: id_x,
                kind: SubstrateKind::Entity,
                namespace: ns.to_string(),
                field: "body".to_string(),
                embedding_model: None,
                vectors: vec![vec2.clone()],
                updated_at: chrono::Utc::now(),
            }])
            .await
            .expect("insert_batch must complete (outer tx must commit regardless)");

        drop(_guard); // explicit drop for clarity; flag already cleared by take()

        assert_eq!(summary.attempted, 1);
        assert_eq!(
            summary.affected, 0,
            "failpoint must prevent INSERT from succeeding"
        );
        assert_eq!(
            summary.failed, 1,
            "failed counter must increment after injected failure"
        );

        // ROLLBACK TO SAVEPOINT must have restored the deleted stale row.
        let present = store
            .batch_exists(&[id_x], ns)
            .await
            .expect("batch_exists after failpoint");
        assert!(
            present.contains(&id_x),
            "ROLLBACK TO SAVEPOINT must restore the stale row after DELETE + injected failure"
        );

        // Self-similarity with vec1 (not vec2) confirms the original bytes are restored.
        let hits = store
            .search(VectorSearchRequest {
                query_vectors: vec![vec1.clone()],
                top_k: 1,
                namespace: Some(ns.to_string()),
                kind: Some(SubstrateKind::Entity),
                embedding_model: None,
                filter: None,
                backend_hints: None,
            })
            .await
            .expect("search after failpoint");

        assert_eq!(
            hits.len(),
            1,
            "stale vector must be searchable after rollback"
        );
        assert_eq!(hits[0].subject_id, id_x);
        let sim = hits[0].score.to_f64();
        assert!(
            sim > 0.999,
            "similarity to vec1 must be ~1.0 (got {sim:.6}); \
             a lower value means the stale embedding was not restored — ROLLBACK TO SAVEPOINT failed"
        );

        // Cross-check: vec2 must NOT be the stored embedding.
        let hits2 = store
            .search(VectorSearchRequest {
                query_vectors: vec![vec2.clone()],
                top_k: 1,
                namespace: Some(ns.to_string()),
                kind: Some(SubstrateKind::Entity),
                embedding_model: None,
                filter: None,
                backend_hints: None,
            })
            .await
            .expect("search vec2 after failpoint");
        let sim2 = hits2.first().map(|h| h.score.to_f64()).unwrap_or(0.0);
        assert!(
            sim2 < 0.99,
            "similarity to vec2 must be < 0.99 (got {sim2:.6}); \
             vec2 must not be the stored embedding after a rolled-back INSERT"
        );
    }

    /// SENTINEL — update: stale row is restored when DELETE succeeds but
    /// INSERT is forced to fail via the cfg(test) failpoint. See
    /// crates/khive-db/docs/api/vectors.md#update_rollback_restores_deleted_stale_after_post_delete_insert_failure
    #[tokio::test]
    async fn update_rollback_restores_deleted_stale_after_post_delete_insert_failure() {
        let pool = make_vec_pool();
        let model_key = "sentinel_upd_rb";
        let dims = 4;
        let ns = "ns:sentinel_upd";

        create_vec_table(&pool, model_key, dims);

        let store = SqliteVecStore::new(
            Arc::clone(&pool),
            false,
            model_key.to_string(),
            model_key.to_string(),
            dims,
            ns.to_string(),
        )
        .expect("SqliteVecStore::new");

        let id_x = Uuid::new_v4();
        let vec1 = vec![0.1f32, 0.2, 0.3, 0.4];
        let vec2 = vec![0.9f32, 0.0, 0.0, 0.0];

        // Insert the stale row that must survive.
        store
            .insert(id_x, SubstrateKind::Entity, ns, "body", vec![vec1.clone()])
            .await
            .expect("stale insert");

        // Arm the failpoint under a RAII guard.
        let _guard = failpoint::FailpointGuard::new();

        // Same namespace, correct dims, finite — DELETE will run, then failpoint fires.
        let result = store
            .update(id_x, SubstrateKind::Entity, ns, "body", vec![vec2.clone()])
            .await;

        drop(_guard);

        assert!(
            result.is_err(),
            "update must propagate the injected error back to the caller"
        );

        // Transaction rollback must have restored the deleted stale row.
        let present = store
            .batch_exists(&[id_x], ns)
            .await
            .expect("batch_exists after failpoint");
        assert!(
            present.contains(&id_x),
            "transaction rollback must restore the stale row after DELETE + injected failure"
        );

        // Self-similarity with vec1 confirms the original bytes are intact.
        let hits = store
            .search(VectorSearchRequest {
                query_vectors: vec![vec1.clone()],
                top_k: 1,
                namespace: Some(ns.to_string()),
                kind: Some(SubstrateKind::Entity),
                embedding_model: None,
                filter: None,
                backend_hints: None,
            })
            .await
            .expect("search after failpoint");

        assert_eq!(
            hits.len(),
            1,
            "stale vector must be searchable after rollback"
        );
        assert_eq!(hits[0].subject_id, id_x);
        let sim = hits[0].score.to_f64();
        assert!(
            sim > 0.999,
            "similarity to vec1 must be ~1.0 (got {sim:.6}); \
             a lower value means the stale embedding was not restored — transaction rollback failed"
        );
    }

    /// #546: `insert` now routes through the shared `replace_vector_row_dml`
    /// helper, so the same post-delete-failpoint rollback guarantee that
    /// covers `update` must also cover `insert`. See
    /// crates/khive-db/docs/api/vectors.md#insert_rollback_restores_deleted_stale_after_post_delete_insert_failure
    #[tokio::test]
    async fn insert_rollback_restores_deleted_stale_after_post_delete_insert_failure() {
        let pool = make_vec_pool();
        let model_key = "sentinel_ins_rb";
        let dims = 4;
        let ns = "ns:sentinel_ins";

        create_vec_table(&pool, model_key, dims);

        let store = SqliteVecStore::new(
            Arc::clone(&pool),
            false,
            model_key.to_string(),
            model_key.to_string(),
            dims,
            ns.to_string(),
        )
        .expect("SqliteVecStore::new");

        let id_x = Uuid::new_v4();
        let vec1 = vec![0.1f32, 0.2, 0.3, 0.4];
        let vec2 = vec![0.9f32, 0.0, 0.0, 0.0];

        // Insert the stale row that must survive a second, failing `insert`
        // call for the same (subject_id, namespace) — `vec0` has no
        // INSERT OR REPLACE, so a second `insert` is itself a replace.
        store
            .insert(id_x, SubstrateKind::Entity, ns, "body", vec![vec1.clone()])
            .await
            .expect("stale insert");

        // Arm the failpoint under a RAII guard.
        let _guard = failpoint::FailpointGuard::new();

        // Same namespace, correct dims, finite — DELETE will run, then failpoint fires.
        let result = store
            .insert(id_x, SubstrateKind::Entity, ns, "body", vec![vec2.clone()])
            .await;

        drop(_guard);

        assert!(
            result.is_err(),
            "insert must propagate the injected error back to the caller"
        );

        // Transaction rollback must have restored the deleted stale row.
        let present = store
            .batch_exists(&[id_x], ns)
            .await
            .expect("batch_exists after failpoint");
        assert!(
            present.contains(&id_x),
            "transaction rollback must restore the stale row after DELETE + injected failure"
        );

        // Self-similarity with vec1 confirms the original bytes are intact.
        let hits = store
            .search(VectorSearchRequest {
                query_vectors: vec![vec1.clone()],
                top_k: 1,
                namespace: Some(ns.to_string()),
                kind: Some(SubstrateKind::Entity),
                embedding_model: None,
                filter: None,
                backend_hints: None,
            })
            .await
            .expect("search after failpoint");

        assert_eq!(
            hits.len(),
            1,
            "stale vector must be searchable after rollback"
        );
        assert_eq!(hits[0].subject_id, id_x);
        let sim = hits[0].score.to_f64();
        assert!(
            sim > 0.999,
            "similarity to vec1 must be ~1.0 (got {sim:.6}); \
             a lower value means the stale embedding was not restored — transaction rollback failed"
        );
    }
}

// ---------------------------------------------------------------------------
// Orphan sweep tests
// ---------------------------------------------------------------------------
// Require the `vectors` feature because the sweep queries the vec0 virtual
// table, which only exists when the sqlite-vec extension is loaded.
// ---------------------------------------------------------------------------
#[cfg(all(test, feature = "vectors"))]
mod orphan_sweep_tests {
    use std::sync::Arc;

    use khive_storage::types::{OrphanSweepConfig, OrphanSweepResult};
    use khive_storage::VectorStore;
    use khive_types::SubstrateKind;
    use uuid::Uuid;

    use super::*;

    // ── helpers ──────────────────────────────────────────────────────────────

    fn make_pool() -> Arc<crate::pool::ConnectionPool> {
        use crate::pool::{ConnectionPool, PoolConfig};
        crate::extension::ensure_extensions_loaded();
        Arc::new(
            ConnectionPool::new(PoolConfig {
                path: None,
                ..PoolConfig::default()
            })
            .expect("in-memory pool"),
        )
    }

    /// Create minimal substrate tables (id + deleted_at only — enough for the anti-join).
    fn create_substrate_tables(pool: &Arc<crate::pool::ConnectionPool>) {
        pool.try_writer()
            .expect("writer")
            .conn()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS entities \
                     (id TEXT PRIMARY KEY, deleted_at INTEGER); \
                 CREATE TABLE IF NOT EXISTS notes \
                     (id TEXT PRIMARY KEY, deleted_at INTEGER);",
            )
            .expect("create substrate tables");
    }

    fn create_vec_table(pool: &Arc<crate::pool::ConnectionPool>, model_key: &str, dims: usize) {
        let ddl = format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS vec_{} USING vec0(\
             subject_id TEXT PRIMARY KEY, \
             namespace TEXT NOT NULL, \
             kind TEXT NOT NULL, \
             field TEXT NOT NULL, \
             embedding_model TEXT NOT NULL, \
             embedding float[{}] distance_metric=cosine)",
            model_key, dims
        );
        let writer = pool.try_writer().expect("writer");
        writer.conn().execute_batch(&ddl).expect("create vec table");
        writer
            .conn()
            .execute_batch(crate::migrations::ANN_WRITE_LOG_DDL)
            .expect("create ann_write_log");
    }

    fn make_store(
        pool: Arc<crate::pool::ConnectionPool>,
        model_key: &str,
        dims: usize,
        ns: &str,
    ) -> SqliteVecStore {
        SqliteVecStore::new(
            pool,
            false,
            model_key.to_string(),
            model_key.to_string(),
            dims,
            ns.to_string(),
        )
        .expect("SqliteVecStore::new")
    }

    /// Insert a substrate row into `entities`.  `deleted_at = None` → live; `Some(ts)` → soft-deleted.
    fn insert_entity(pool: &Arc<crate::pool::ConnectionPool>, id: Uuid, deleted_at: Option<i64>) {
        let id_str = id.to_string();
        pool.try_writer()
            .expect("writer")
            .conn()
            .execute(
                "INSERT INTO entities (id, deleted_at) VALUES (?1, ?2)",
                rusqlite::params![id_str, deleted_at],
            )
            .expect("insert entity");
    }

    fn vec4(a: f32, b: f32, c: f32, d: f32) -> Vec<f32> {
        vec![a, b, c, d]
    }

    fn sweep_all(max_delete: u32, dry_run: bool) -> OrphanSweepConfig {
        OrphanSweepConfig {
            subject_id_allowlist: None,
            namespaces: vec![],
            substrate_kinds: vec![],
            max_delete,
            dry_run,
        }
    }

    // ── test 1: live subject → vector kept ───────────────────────────────────

    #[tokio::test]
    async fn orphan_sweep_keeps_live_subject() {
        let pool = make_pool();
        create_substrate_tables(&pool);
        create_vec_table(&pool, "sw_live", 4);
        let store = make_store(Arc::clone(&pool), "sw_live", 4, "ns:sw");
        let ns = "ns:sw";

        let id = Uuid::new_v4();
        insert_entity(&pool, id, None); // live

        store
            .insert(
                id,
                SubstrateKind::Entity,
                ns,
                "body",
                vec![vec4(0.1, 0.2, 0.3, 0.4)],
            )
            .await
            .expect("insert vec");

        let r: OrphanSweepResult = store
            .orphan_sweep(&sweep_all(100, false))
            .await
            .expect("sweep");

        assert_eq!(r.scanned, 1, "one vec row exists");
        assert_eq!(r.would_delete, 0, "live subject is not an orphan");
        assert_eq!(r.deleted, 0);
        assert!(!r.max_delete_hit);

        let present = store.batch_exists(&[id], ns).await.expect("exists");
        assert!(present.contains(&id), "live subject's vec must survive");
    }

    // ── test 2: soft-deleted subject → vector swept ──────────────────────────

    #[tokio::test]
    async fn orphan_sweep_sweeps_soft_deleted_subject() {
        let pool = make_pool();
        create_substrate_tables(&pool);
        create_vec_table(&pool, "sw_soft", 4);
        let store = make_store(Arc::clone(&pool), "sw_soft", 4, "ns:soft");
        let ns = "ns:soft";

        let id = Uuid::new_v4();
        insert_entity(&pool, id, Some(1_000_000)); // soft-deleted

        store
            .insert(
                id,
                SubstrateKind::Entity,
                ns,
                "body",
                vec![vec4(0.5, 0.5, 0.5, 0.5)],
            )
            .await
            .expect("insert vec");

        let r = store
            .orphan_sweep(&sweep_all(100, false))
            .await
            .expect("sweep");

        assert_eq!(r.scanned, 1);
        assert_eq!(r.would_delete, 1, "soft-deleted subject counts as orphan");
        assert_eq!(r.deleted, 1);
        assert!(!r.max_delete_hit);

        let present = store.batch_exists(&[id], ns).await.expect("exists");
        assert!(
            !present.contains(&id),
            "soft-deleted subject's vec must be swept"
        );
    }

    // ── test 3: absent subject → vector swept ────────────────────────────────

    #[tokio::test]
    async fn orphan_sweep_sweeps_absent_subject() {
        let pool = make_pool();
        create_substrate_tables(&pool);
        create_vec_table(&pool, "sw_absent", 4);
        let store = make_store(Arc::clone(&pool), "sw_absent", 4, "ns:absent");
        let ns = "ns:absent";

        let id = Uuid::new_v4(); // no substrate row at all

        store
            .insert(
                id,
                SubstrateKind::Entity,
                ns,
                "body",
                vec![vec4(0.1, 0.2, 0.3, 0.4)],
            )
            .await
            .expect("insert vec");

        let r = store
            .orphan_sweep(&sweep_all(100, false))
            .await
            .expect("sweep");

        assert_eq!(r.scanned, 1);
        assert_eq!(r.would_delete, 1, "absent subject counts as orphan");
        assert_eq!(r.deleted, 1);

        let present = store.batch_exists(&[id], ns).await.expect("exists");
        assert!(!present.contains(&id), "absent subject's vec must be swept");
    }

    // ── test 4: dry_run → nothing deleted, would_delete populated ────────────

    #[tokio::test]
    async fn orphan_sweep_dry_run_does_not_delete() {
        let pool = make_pool();
        create_substrate_tables(&pool);
        create_vec_table(&pool, "sw_dry", 4);
        let store = make_store(Arc::clone(&pool), "sw_dry", 4, "ns:dry");
        let ns = "ns:dry";

        let id = Uuid::new_v4(); // absent subject → orphan
        store
            .insert(
                id,
                SubstrateKind::Entity,
                ns,
                "body",
                vec![vec4(0.1, 0.2, 0.3, 0.4)],
            )
            .await
            .expect("insert vec");

        let r = store
            .orphan_sweep(&sweep_all(100, true))
            .await
            .expect("sweep");

        assert_eq!(r.would_delete, 1, "dry-run must still count the orphan");
        assert_eq!(r.deleted, 0, "dry-run must not delete anything");

        let present = store.batch_exists(&[id], ns).await.expect("exists");
        assert!(present.contains(&id), "dry-run must not remove the vec");
    }

    // ── test 5: max_delete cap ────────────────────────────────────────────────

    #[tokio::test]
    async fn orphan_sweep_max_delete_caps_deletion() {
        let pool = make_pool();
        create_substrate_tables(&pool);
        create_vec_table(&pool, "sw_cap", 4);
        let store = make_store(Arc::clone(&pool), "sw_cap", 4, "ns:cap");
        let ns = "ns:cap";

        // Insert 5 orphaned vecs (no substrate rows).
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        for (i, &id) in ids.iter().enumerate() {
            let v = i as f32 / 10.0;
            store
                .insert(
                    id,
                    SubstrateKind::Entity,
                    ns,
                    "body",
                    vec![vec![v, v + 0.1, v + 0.2, v + 0.3]],
                )
                .await
                .expect("insert vec");
        }

        let r = store
            .orphan_sweep(&OrphanSweepConfig {
                subject_id_allowlist: None,
                namespaces: vec![],
                substrate_kinds: vec![],
                max_delete: 2,
                dry_run: false,
            })
            .await
            .expect("sweep");

        assert_eq!(r.scanned, 5);
        assert_eq!(r.would_delete, 5);
        assert_eq!(r.deleted, 2, "cap must stop at max_delete");
        assert!(
            r.max_delete_hit,
            "max_delete_hit must be true when cap triggered"
        );

        // Verify exactly 3 vecs survive.
        let mut surviving = 0usize;
        for &id in &ids {
            if store
                .batch_exists(&[id], ns)
                .await
                .expect("exists")
                .contains(&id)
            {
                surviving += 1;
            }
        }
        assert_eq!(surviving, 3, "3 orphans must survive after cap");
    }

    // ── test 6: namespace filter ──────────────────────────────────────────────

    #[tokio::test]
    async fn orphan_sweep_namespace_filter_scopes_sweep() {
        let pool = make_pool();
        create_substrate_tables(&pool);
        create_vec_table(&pool, "sw_ns", 4);
        let store = make_store(Arc::clone(&pool), "sw_ns", 4, "ns:a");

        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();

        store
            .insert(
                id_a,
                SubstrateKind::Entity,
                "ns:a",
                "body",
                vec![vec4(0.1, 0.2, 0.3, 0.4)],
            )
            .await
            .expect("insert ns:a");
        store
            .insert(
                id_b,
                SubstrateKind::Entity,
                "ns:b",
                "body",
                vec![vec4(0.5, 0.6, 0.7, 0.8)],
            )
            .await
            .expect("insert ns:b");

        // Both are orphans (no substrate rows); sweep scoped to ns:a only.
        let r = store
            .orphan_sweep(&OrphanSweepConfig {
                subject_id_allowlist: None,
                namespaces: vec!["ns:a".to_string()],
                substrate_kinds: vec![],
                max_delete: 100,
                dry_run: false,
            })
            .await
            .expect("sweep");

        assert_eq!(r.scanned, 1, "only ns:a row visible to scoped sweep");
        assert_eq!(r.deleted, 1);

        let exists_a = store.batch_exists(&[id_a], "ns:a").await.expect("exists a");
        let exists_b = store.batch_exists(&[id_b], "ns:b").await.expect("exists b");
        assert!(!exists_a.contains(&id_a), "ns:a orphan must be swept");
        assert!(exists_b.contains(&id_b), "ns:b vec must be untouched");
    }

    // ── test 7: substrate_kinds filter ───────────────────────────────────────

    #[tokio::test]
    async fn orphan_sweep_substrate_kinds_filter_scopes_sweep() {
        let pool = make_pool();
        create_substrate_tables(&pool);
        create_vec_table(&pool, "sw_kind", 4);
        let store = make_store(Arc::clone(&pool), "sw_kind", 4, "ns:kind");
        let ns = "ns:kind";

        let id_ent = Uuid::new_v4();
        let id_note = Uuid::new_v4();

        // Both orphaned; one entity-kind vec, one note-kind vec.
        store
            .insert(
                id_ent,
                SubstrateKind::Entity,
                ns,
                "body",
                vec![vec4(0.1, 0.2, 0.3, 0.4)],
            )
            .await
            .expect("insert entity vec");
        store
            .insert(
                id_note,
                SubstrateKind::Note,
                ns,
                "body",
                vec![vec4(0.5, 0.6, 0.7, 0.8)],
            )
            .await
            .expect("insert note vec");

        // Sweep only entity-kind vecs.
        let r = store
            .orphan_sweep(&OrphanSweepConfig {
                subject_id_allowlist: None,
                namespaces: vec![],
                substrate_kinds: vec![SubstrateKind::Entity],
                max_delete: 100,
                dry_run: false,
            })
            .await
            .expect("sweep");

        assert_eq!(r.scanned, 1, "kind filter restricts scanned count");
        assert_eq!(r.deleted, 1, "only entity-kind orphan is swept");

        let ent_exists = store.batch_exists(&[id_ent], ns).await.expect("ent exists");
        let note_exists = store
            .batch_exists(&[id_note], ns)
            .await
            .expect("note exists");
        assert!(
            !ent_exists.contains(&id_ent),
            "entity-kind orphan must be swept"
        );
        assert!(
            note_exists.contains(&id_note),
            "note-kind vec must be untouched"
        );
    }

    // ── test 8: subject_id_allowlist filter ──────────────────────────────────

    #[tokio::test]
    async fn orphan_sweep_allowlist_restricts_eligible_rows() {
        let pool = make_pool();
        create_substrate_tables(&pool);
        create_vec_table(&pool, "sw_allow", 4);
        let store = make_store(Arc::clone(&pool), "sw_allow", 4, "ns:allow");
        let ns = "ns:allow";

        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let id3 = Uuid::new_v4(); // not in allowlist

        for (i, &id) in [id1, id2, id3].iter().enumerate() {
            let v = i as f32 * 0.1 + 0.1;
            store
                .insert(
                    id,
                    SubstrateKind::Entity,
                    ns,
                    "body",
                    vec![vec![v, v, v, v]],
                )
                .await
                .expect("insert vec");
        }

        // All are orphans; allowlist only allows id1 and id2 to be swept.
        let r = store
            .orphan_sweep(&OrphanSweepConfig {
                subject_id_allowlist: Some(vec![id1, id2]),
                namespaces: vec![],
                substrate_kinds: vec![],
                max_delete: 100,
                dry_run: false,
            })
            .await
            .expect("sweep");

        assert_eq!(r.scanned, 2, "allowlist restricts scanned to 2");
        assert_eq!(r.would_delete, 2);
        assert_eq!(r.deleted, 2, "both allowlisted orphans deleted");

        let e1 = store.batch_exists(&[id1], ns).await.expect("e1");
        let e2 = store.batch_exists(&[id2], ns).await.expect("e2");
        let e3 = store.batch_exists(&[id3], ns).await.expect("e3");
        assert!(!e1.contains(&id1), "id1 must be swept");
        assert!(!e2.contains(&id2), "id2 must be swept");
        assert!(e3.contains(&id3), "id3 not in allowlist must survive");
    }

    // ── helpers for note substrate rows ─────────────────────────────────────

    fn insert_note(pool: &Arc<crate::pool::ConnectionPool>, id: Uuid, deleted_at: Option<i64>) {
        let id_str = id.to_string();
        pool.try_writer()
            .expect("writer")
            .conn()
            .execute(
                "INSERT INTO notes (id, deleted_at) VALUES (?1, ?2)",
                rusqlite::params![id_str, deleted_at],
            )
            .expect("insert note");
    }

    // ── test 9: live note → vector kept ──────────────────────────────────────

    #[tokio::test]
    async fn orphan_sweep_keeps_live_note() {
        let pool = make_pool();
        create_substrate_tables(&pool);
        create_vec_table(&pool, "sw_note_live", 4);
        let store = make_store(Arc::clone(&pool), "sw_note_live", 4, "ns:nlive");
        let ns = "ns:nlive";

        let id = Uuid::new_v4();
        insert_note(&pool, id, None); // live note row

        store
            .insert(
                id,
                SubstrateKind::Note,
                ns,
                "body",
                vec![vec4(0.1, 0.2, 0.3, 0.4)],
            )
            .await
            .expect("insert vec");

        let r = store
            .orphan_sweep(&sweep_all(100, false))
            .await
            .expect("sweep");

        assert_eq!(r.scanned, 1);
        assert_eq!(r.would_delete, 0, "live note is not an orphan");
        assert_eq!(r.deleted, 0);

        let present = store.batch_exists(&[id], ns).await.expect("exists");
        assert!(present.contains(&id), "live note's vec must survive");
    }

    // ── test 10: soft-deleted note → vector swept ─────────────────────────────

    #[tokio::test]
    async fn orphan_sweep_sweeps_soft_deleted_note() {
        let pool = make_pool();
        create_substrate_tables(&pool);
        create_vec_table(&pool, "sw_note_soft", 4);
        let store = make_store(Arc::clone(&pool), "sw_note_soft", 4, "ns:nsoft");
        let ns = "ns:nsoft";

        let id = Uuid::new_v4();
        insert_note(&pool, id, Some(1_000_000)); // soft-deleted note row

        store
            .insert(
                id,
                SubstrateKind::Note,
                ns,
                "body",
                vec![vec4(0.5, 0.5, 0.5, 0.5)],
            )
            .await
            .expect("insert vec");

        let r = store
            .orphan_sweep(&sweep_all(100, false))
            .await
            .expect("sweep");

        assert_eq!(r.scanned, 1);
        assert_eq!(r.would_delete, 1, "soft-deleted note counts as orphan");
        assert_eq!(r.deleted, 1);

        let present = store.batch_exists(&[id], ns).await.expect("exists");
        assert!(
            !present.contains(&id),
            "soft-deleted note's vec must be swept"
        );
    }

    // ── test 11: mid-transaction error must NOT poison the pooled connection ──
    //
    // Regression for the transaction-leak bug: if orphan_sweep errors after
    // BEGIN IMMEDIATE but before COMMIT, the pooled writer must NOT be left
    // with an open transaction.  Without the RAII guard, the next writer
    // call fails with "cannot start a transaction within a transaction".
    //
    // Deterministic injection: we create the vec table but deliberately omit
    // the substrate tables.  The anti-join queries reference `entities` and
    // `notes`, so the first scan COUNT fails with "no such table: entities".
    // After the error, we immediately perform a normal vector insert on the
    // same store and assert it succeeds — proving the connection is clean.

    #[tokio::test]
    async fn orphan_sweep_error_does_not_poison_connection() {
        let pool = make_pool();
        // Note: create_substrate_tables is intentionally NOT called here.
        create_vec_table(&pool, "sw_poison", 4);
        let store = make_store(Arc::clone(&pool), "sw_poison", 4, "ns:poison");
        let ns = "ns:poison";

        // orphan_sweep must fail because `entities` / `notes` do not exist.
        let sweep_result = store.orphan_sweep(&sweep_all(100, false)).await;
        assert!(
            sweep_result.is_err(),
            "sweep must fail when substrate tables are absent"
        );

        // The connection must not be poisoned: a normal vector insert must succeed.
        let id = Uuid::new_v4();
        store
            .insert(
                id,
                SubstrateKind::Entity,
                ns,
                "body",
                vec![vec4(0.1, 0.2, 0.3, 0.4)],
            )
            .await
            .expect("insert after failed sweep must succeed (connection not poisoned)");

        let present = store.batch_exists(&[id], ns).await.expect("exists");
        assert!(
            present.contains(&id),
            "vector inserted after failed sweep must be present"
        );
    }
}

/// ADR-067 Component A entry 7 / Amendment 1: `insert_batch` and
/// `orphan_sweep` are the `BEGIN IMMEDIATE`-issuing sites in this store that
/// route through the pool-wide `WriterTask` when the write queue is enabled
/// (`insert`/`update` route through `vec_upsert_atomic_dml`'s SAVEPOINT
/// instead — see the flag-on branches in the `VectorStore` impl above).
/// Needs the real `vec0` extension loaded, so it lives behind the same
/// `feature = "vectors"` gate as its sibling
/// `atomic_replace_tests`/`orphan_sweep_tests` modules — `cargo test
/// --workspace` (no `--all-features`) does not compile or run it, matching
/// the existing convention in this file.
#[cfg(all(test, feature = "vectors"))]
mod write_queue_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use khive_storage::types::VectorRecord;
    use khive_storage::VectorStore;
    use khive_types::SubstrateKind;
    use uuid::Uuid;

    use super::*;
    use crate::pool::{ConnectionPool, PoolConfig};

    fn create_vec_table(pool: &Arc<ConnectionPool>, model_key: &str, dims: usize) {
        let ddl = format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS vec_{} USING vec0(\
             subject_id TEXT PRIMARY KEY, \
             namespace TEXT NOT NULL, \
             kind TEXT NOT NULL, \
             field TEXT NOT NULL, \
             embedding_model TEXT NOT NULL, \
             embedding float[{}] distance_metric=cosine)",
            model_key, dims
        );
        let writer = pool.writer().expect("writer");
        writer.conn().execute_batch(&ddl).expect("create vec table");
        writer
            .conn()
            .execute_batch(crate::migrations::ANN_WRITE_LOG_DDL)
            .expect("create ann_write_log");
    }

    /// Constructed via a `PoolConfig` literal (`write_queue_enabled: true`),
    /// not the `KHIVE_WRITE_QUEUE` env var — that env var is process-global
    /// and this crate's other tests are NOT `#[serial]` against it, so a
    /// window where it is set here could leak into a
    /// concurrently-scheduled test's own pool construction (ADR-067
    /// Component A). Builds the pool inline (rather than
    /// via `make_file_backed_pool`, which hardcodes `PoolConfig::default()`)
    /// so `write_queue_enabled` can be set directly in the literal.
    #[tokio::test]
    async fn insert_batch_routes_through_writer_task_when_flag_enabled() {
        crate::extension::ensure_extensions_loaded();

        let model_key = "write_queue_flag_test";
        let dims = 4usize;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("write_queue_vectors.db");
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: Some(path),
                write_queue_enabled: true,
                ..PoolConfig::default()
            })
            .expect("file-backed pool"),
        );
        create_vec_table(&pool, model_key, dims);

        let store = SqliteVecStore::new(
            Arc::clone(&pool),
            true,
            model_key.to_string(),
            model_key.to_string(),
            dims,
            "ns:test".to_string(),
        )
        .expect("SqliteVecStore::new");

        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let records = vec![
            VectorRecord {
                subject_id: id1,
                kind: SubstrateKind::Entity,
                namespace: "ns:test".to_string(),
                field: "body".to_string(),
                embedding_model: None,
                vectors: vec![vec![0.1, 0.2, 0.3, 0.4]],
                updated_at: chrono::Utc::now(),
            },
            VectorRecord {
                subject_id: id2,
                kind: SubstrateKind::Entity,
                namespace: "ns:test".to_string(),
                field: "body".to_string(),
                embedding_model: None,
                vectors: vec![vec![0.5, 0.6, 0.7, 0.8]],
                updated_at: chrono::Utc::now(),
            },
        ];

        let summary = store.insert_batch(records).await.unwrap();
        assert_eq!(summary.attempted, 2);
        assert_eq!(summary.affected, 2);
        assert_eq!(summary.failed, 0);

        let present = store
            .batch_exists(&[id1, id2], "ns:test")
            .await
            .expect("batch_exists");
        assert!(present.contains(&id1));
        assert!(present.contains(&id2));
        assert_eq!(
            pool.writer_task_spawn_count(),
            1,
            "the flag-ON path must actually spawn and use the writer task"
        );
    }

    /// Create minimal substrate tables (id + deleted_at only — enough for the
    /// anti-join). Mirrors `orphan_sweep_tests::create_substrate_tables`;
    /// duplicated here (rather than shared) because that helper is private to
    /// its own sibling module — same convention as this module's own
    /// `create_vec_table` duplicate.
    fn create_substrate_tables(pool: &Arc<ConnectionPool>) {
        pool.try_writer()
            .expect("writer")
            .conn()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS entities \
                     (id TEXT PRIMARY KEY, deleted_at INTEGER); \
                 CREATE TABLE IF NOT EXISTS notes \
                     (id TEXT PRIMARY KEY, deleted_at INTEGER);",
            )
            .expect("create substrate tables");
    }

    /// Insert a substrate row into `entities`. `deleted_at = None` → live.
    fn insert_entity(pool: &Arc<ConnectionPool>, id: Uuid, deleted_at: Option<i64>) {
        let id_str = id.to_string();
        pool.try_writer()
            .expect("writer")
            .conn()
            .execute(
                "INSERT INTO entities (id, deleted_at) VALUES (?1, ?2)",
                rusqlite::params![id_str, deleted_at],
            )
            .expect("insert entity");
    }

    /// ADR-067 Amendment 1: `orphan_sweep`'s flag-on path must route through
    /// the pool-wide `WriterTask` (not `with_writer_unmanaged`'s pool-mutex
    /// path) when the write queue is enabled — mirrors
    /// `insert_batch_routes_through_writer_task_when_flag_enabled` above.
    #[tokio::test]
    async fn orphan_sweep_routes_through_writer_task_when_flag_enabled() {
        crate::extension::ensure_extensions_loaded();

        let model_key = "write_queue_orphan_sweep";
        let dims = 4usize;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("write_queue_orphan_sweep.db");
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: Some(path),
                write_queue_enabled: true,
                ..PoolConfig::default()
            })
            .expect("file-backed pool"),
        );
        create_substrate_tables(&pool);
        create_vec_table(&pool, model_key, dims);

        let store = SqliteVecStore::new(
            Arc::clone(&pool),
            true,
            model_key.to_string(),
            model_key.to_string(),
            dims,
            "ns:test".to_string(),
        )
        .expect("SqliteVecStore::new");

        let live_id = Uuid::new_v4();
        insert_entity(&pool, live_id, None); // live subject
        let orphan_id = Uuid::new_v4(); // no substrate row -> orphaned vector

        store
            .insert(
                live_id,
                SubstrateKind::Entity,
                "ns:test",
                "body",
                vec![vec![0.1, 0.2, 0.3, 0.4]],
            )
            .await
            .expect("insert live vector");
        store
            .insert(
                orphan_id,
                SubstrateKind::Entity,
                "ns:test",
                "body",
                vec![vec![0.5, 0.6, 0.7, 0.8]],
            )
            .await
            .expect("insert orphan vector");

        // Dry run: reports the orphan without deleting it.
        let dry = store
            .orphan_sweep(&OrphanSweepConfig {
                subject_id_allowlist: None,
                namespaces: vec![],
                substrate_kinds: vec![],
                max_delete: 100,
                dry_run: true,
            })
            .await
            .expect("dry-run sweep");
        assert_eq!(dry.scanned, 2);
        assert_eq!(dry.would_delete, 1);
        assert_eq!(dry.deleted, 0);
        assert!(!dry.max_delete_hit);

        // Real sweep: deletes the orphan, keeps the live vector.
        let real = store
            .orphan_sweep(&OrphanSweepConfig {
                subject_id_allowlist: None,
                namespaces: vec![],
                substrate_kinds: vec![],
                max_delete: 100,
                dry_run: false,
            })
            .await
            .expect("real sweep");
        assert_eq!(real.scanned, 2);
        assert_eq!(real.would_delete, 1);
        assert_eq!(real.deleted, 1);
        assert!(!real.max_delete_hit);

        let present = store
            .batch_exists(&[live_id, orphan_id], "ns:test")
            .await
            .expect("batch_exists");
        assert!(
            present.contains(&live_id),
            "live vector must survive the sweep"
        );
        assert!(
            !present.contains(&orphan_id),
            "orphaned vector must be swept"
        );

        // `writer_task_spawn_count() == 1` alone does not discriminate the
        // fix from a regression: `SqliteVecStore::new` and the two setup
        // `store.insert(..)` calls above already spawn and use the writer
        // task, so that counter would read 1 even if `orphan_sweep` itself
        // had reverted to the legacy `with_writer_unmanaged` path. Prove
        // routing directly instead, mirroring
        // `upsert_entity_routes_through_writer_task_when_flag_enabled`
        // (entity_tests.rs): hold the writer task's single drain slot open
        // with an occupier parked on a oneshot (`blocking_recv`, valid
        // inside the writer task's `spawn_blocking`), then call
        // `orphan_sweep` on a separate task and poll
        // `WriterTaskHandle::queue_depth()`. A version that genuinely
        // routes through `writer_task.send(..)` must show the request
        // sitting in the channel (`queue_depth() >= 1`) while the occupier
        // holds the slot; a version that fell back to
        // `with_writer_unmanaged`'s pool-mutex path never touches this
        // channel, so `queue_depth()` would stay `0` for the whole poll
        // window — the failure mode this test exists to catch.
        let writer_task = pool
            .writer_task_handle()
            .expect("writer task handle")
            .expect("writer task must be spawned for a file-backed pool with the flag on");

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

        let sweep_task = tokio::spawn(async move {
            store
                .orphan_sweep(&OrphanSweepConfig {
                    subject_id_allowlist: None,
                    namespaces: vec![],
                    substrate_kinds: vec![],
                    max_delete: 100,
                    dry_run: true,
                })
                .await
        });

        let mut saw_enqueued = false;
        for _ in 0..100 {
            if writer_task.queue_depth() >= 1 {
                saw_enqueued = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            saw_enqueued,
            "orphan_sweep's write request never appeared in the writer task's channel \
             while the occupier held the single drain slot — orphan_sweep is not routing \
             through the shared writer task"
        );

        release_tx
            .send(())
            .expect("occupier must still be waiting on the release signal");
        occupier
            .await
            .expect("occupier task must not panic")
            .expect("occupier write must succeed");
        let post_sweep = sweep_task
            .await
            .expect("sweep task must not panic")
            .expect("orphan_sweep must succeed once unblocked");
        assert_eq!(
            post_sweep.scanned, 1,
            "only the surviving live vector remains after the earlier real sweep"
        );
    }

    /// Revert-and-confirm-fails companion (mirrors the pattern in
    /// `crates/khive-vcs/src/sync.rs::checkpoint_wal_write_queue_tests`): the
    /// OLD `orphan_sweep` shape — a closure that opens its own
    /// `Transaction::new_unchecked`/`BEGIN IMMEDIATE` — must fail if routed
    /// through the WriterTask channel. `run_writer_task`'s drain loop already
    /// wraps every request in its own `BEGIN IMMEDIATE` before invoking the
    /// closure, so a second `BEGIN IMMEDIATE` issued from inside the closure
    /// violates SQLite's nested-transaction rule. This proves the fix's
    /// DML-only extraction (`orphan_sweep_dml`, no inner `BEGIN`) is
    /// required — naively forwarding the old closure to `writer_task.send()`
    /// would not have worked.
    #[tokio::test]
    async fn orphan_sweep_old_unmanaged_shape_nests_transaction_under_write_queue() {
        crate::extension::ensure_extensions_loaded();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("write_queue_orphan_sweep_regression.db");
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: Some(path),
                write_queue_enabled: true,
                ..PoolConfig::default()
            })
            .expect("file-backed pool"),
        );
        create_substrate_tables(&pool);
        create_vec_table(&pool, "write_queue_orphan_sweep_regression", 4);

        let writer_task = pool
            .writer_task_handle()
            .expect("writer task handle")
            .expect("writer task must spawn for a file-backed pool with the flag on");

        let result: Result<(), StorageError> = writer_task
            .send(move |conn| {
                // The OLD orphan_sweep shape: opens its own BEGIN IMMEDIATE via
                // `Transaction::new_unchecked`. Under the write queue this
                // closure already runs inside the drain loop's own open
                // transaction, so this must fail with SQLite's
                // nested-transaction error.
                let tx = rusqlite::Transaction::new_unchecked(
                    conn,
                    rusqlite::TransactionBehavior::Immediate,
                )
                .map_err(|e| map_err(e, "orphan_sweep_old_shape"))?;
                tx.commit()
                    .map_err(|e| map_err(e, "orphan_sweep_old_shape"))?;
                Ok(())
            })
            .await;

        let err = result.expect_err(
            "routing the OLD orphan_sweep closure (its own BEGIN IMMEDIATE) through the \
             WriterTask must fail under KHIVE_WRITE_QUEUE — if this now succeeds, re-audit \
             whether the WriterTask still owns the sole BEGIN IMMEDIATE for this connection",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("cannot start a transaction within a transaction"),
            "expected the deterministic nested-transaction failure (SQLite's own message \
             for a second BEGIN issued inside an already-open transaction), got: {msg}"
        );
    }
}
