//! sqlite-vec backed `VectorStore`: one vec0 table per embedding model, scoped to namespace.

#[path = "vectors/provenance.rs"]
mod provenance;
use provenance::{provenance_read_sql, provenance_sidecar_exists};

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::OptionalExtension;
use uuid::Uuid;

use khive_score::{cmp_desc_then_id, try_cosine_score_with_f32_tolerance, DeterministicScore};
use khive_storage::error::StorageError;
use khive_storage::types::{
    BatchWriteErrorClass, BatchWriteRetryability, BatchWriteSummary, IndexRebuildScope,
    OrphanSweepConfig, OrphanSweepResult, SqlStatement, SqlValue, VectorIndexKind,
    VectorProvenance, VectorRecord, VectorSearchHit, VectorSearchRequest, VectorStoreCapabilities,
    VectorStoreInfo,
};
use khive_storage::StorageResult;
use khive_storage::VectorStore;
use khive_storage::{ContentRef, StorageCapability};
use khive_types::SubstrateKind;

use crate::error::SqliteError;
use crate::pool::ConnectionPool;
use crate::sql_bridge::bind_params;
use crate::writer_task::execute_wrapped_transaction;

/// The exact `DELETE` this store's `delete` issues, for a given vector table
/// (ADR-099 B3 r6 structural cut — see `stores::entity`'s sibling block).
/// `table` must already be a trusted, sanitized table name (mirrors
/// `delete`'s own pre-existing lack of a placeholder for table names).
pub(crate) fn delete_vector_statement(
    table: &str,
    subject_id: Uuid,
    namespace: &str,
) -> SqlStatement {
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
    e.into_storage_error(StorageCapability::Vectors, op)
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

/// Convert sqlite-vec's cosine distance through the canonical score contract.
///
/// sqlite-vec accumulates `dot`/`aMag`/`bMag` in `f32` (see
/// `distance_cosine_float` in sqlite-vec.c) and only widens the final result
/// to SQLite's `REAL` (f64) on the way out. The roundoff at a mathematically
/// exact endpoint therefore lands on the `f32` ULP scale — about
/// `f32::EPSILON` (~1.19e-7) for a self- or exactly-opposite comparison,
/// not the `f64::EPSILON` (~2.22e-16) scale of the widening cast itself.
/// Normalize only that f32-scale boundary roundoff, then route through the
/// strict canonical f32 score contract.
fn sqlite_cosine_score(distance: f64) -> Result<DeterministicScore, rusqlite::Error> {
    let conversion_error = |error| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Real, Box::new(error))
    };
    try_cosine_score_with_f32_tolerance(distance).map_err(conversion_error)
}

#[cfg(test)]
mod sqlite_cosine_score_tests;

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
        _is_file_backed: bool,
        model_key: String,
        embedding_model: String,
        dimensions: usize,
        namespace: String,
    ) -> Result<Self, SqliteError> {
        validate_model_key(&model_key)?;
        let table_name = format!("vec_{}", model_key);
        // Enabled by default for file-backed pools. Construction stays
        // synchronous (ADR-067 Component A, mirrors entity.rs policy): a
        // missing writer task is cached without failing construction. Every
        // write re-resolves it and applies strict/compatibility policy then.
        let writer_task = pool.writer_task_handle().ok().flatten();
        Ok(Self {
            pool,
            model_key,
            embedding_model,
            dimensions,
            table_name,
            namespace,
            writer_task,
        })
    }

    /// Re-derive writer-task availability at write time instead of trusting
    /// only the field cached at construction (ADR-136 D1 gate 3 amendment).
    /// `self.writer_task` permanently caches `None` when this store was
    /// constructed outside a Tokio runtime (`writer_task_handle()` returns
    /// `Err(WriterTaskNoRuntime)`, which construction collapses via
    /// `.ok().flatten()`) — every later write, even ones running inside a
    /// runtime, would otherwise silently keep bypassing an enabled queue.
    /// The pool helper also enforces strict fail-closed routing and preserves
    /// a typed `WriterTaskNoRuntime` error when no runtime is available.
    fn current_writer_task(
        &self,
        operation: &'static str,
    ) -> Result<Option<crate::writer_task::WriterTaskHandle>, StorageError> {
        self.pool
            .writer_task_for_write(self.writer_task.as_ref(), operation)
    }

    /// Route a single-row DML-only write through the pool-wide `WriterTask`
    /// when available, else fall back to `with_writer_unmanaged`. See
    /// crates/khive-db/docs/api/vectors.md#with_writer--with_writer_unmanaged--writertask-routing-adr-067-component-a-fork-c-slice-2
    async fn with_writer<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        if let Some(writer_task) = self.current_writer_task(op)? {
            return writer_task
                .send_bounded(move |conn| f(conn).map_err(|e| map_err(e, op)))
                .await;
        }

        self.pool
            .record_direct_route(crate::timeout_sink::Site::DirectRouteVecGeneralWrite);
        self.with_writer_unmanaged(op, f).await
    }

    /// Direct pool-mutex write path; bypasses the WriterTask channel
    /// unconditionally. Owns one admitted transaction around the closure.
    /// The closure must contain DML only. See
    /// crates/khive-db/docs/api/vectors.md#with_writer--with_writer_unmanaged--writertask-routing-adr-067-component-a-fork-c-slice-2
    async fn with_writer_unmanaged<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let guard = pool
                .transaction_write_unit()
                .map_err(|e| map_sqlite_err(e, op))
                .inspect_err(|error| pool.record_direct_writer_error(error))?;
            let conn = guard.conn();
            let _tx_handle = khive_storage::tx_registry::register_scoped(
                Some(format!("{op}_tx")),
                pool.origin(),
            );
            let db_label = crate::timeout_sink::db_label(&pool);
            let (result, terminal_state) = execute_wrapped_transaction(conn, op, move |conn| {
                f(conn).map_err(|e| {
                    crate::timeout_sink::maybe_emit_sqlite_full(&db_label, &e);
                    map_err(e, op)
                })
            });
            if terminal_state.is_some() {
                pool.retire_pooled_writer(conn);
            }
            result.inspect_err(|error| pool.record_direct_writer_error(error))
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Vectors, op, e))?
    }

    async fn with_reader<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        super::run_pooled_store_read(
            Arc::clone(&self.pool),
            StorageCapability::Vectors,
            op,
            move |conn| f(conn).map_err(|error| map_err(error, op)),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_one(
        &self,
        subject_id: Uuid,
        kind: SubstrateKind,
        namespace: &str,
        field: &str,
        vectors: Vec<Vec<f32>>,
        record_ann_delta: bool,
        operation: &'static str,
        savepoint_name: &'static str,
    ) -> Result<(), StorageError> {
        if vectors.len() != 1 {
            return Err(StorageError::Unsupported {
                capability: StorageCapability::Vectors,
                operation: operation.into(),
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
            if let Some(index) = non_finite_index(&embedding) {
                return Err(non_finite_vector_error(operation, index, embedding[index]));
            }
        }

        let failpoint_flag = current_failpoint();
        if let Some(writer_task) = self.current_writer_task(operation)? {
            let table_for_write = table.clone();
            let namespace_for_write = namespace.clone();
            let field_for_write = field.clone();
            let kind_for_write = kind_str.clone();
            let model_for_write = embedding_model.clone();
            let embedding_for_write = embedding.clone();
            return writer_task
                .send_bounded(move |connection| {
                    vec_upsert_atomic_dml(
                        connection,
                        &table_for_write,
                        dims,
                        subject_id,
                        &kind_for_write,
                        &namespace_for_write,
                        &field_for_write,
                        &model_for_write,
                        &embedding_for_write,
                        savepoint_name,
                        record_ann_delta,
                        failpoint_flag,
                    )
                    .map_err(|error| map_err(error, operation))
                })
                .await;
        }

        self.with_writer(operation, move |connection| {
            replace_vector_row_dml(
                connection,
                &table,
                dims,
                VectorRowRef {
                    subject_id,
                    namespace: &namespace,
                    kind: &kind_str,
                    field: &field,
                    embedding_model: &embedding_model,
                    embedding: &embedding,
                    text_fingerprint: None,
                    updated_at: None,
                },
                record_ann_delta,
                failpoint_flag,
            )
        })
        .await
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
    text_fingerprint: Option<&'a ContentRef>,
    updated_at: Option<&'a DateTime<Utc>>,
}

/// Shared DELETE-then-INSERT replacement DML for a single vector row (#546);
/// caller owns the enclosing transaction/savepoint. See
/// crates/khive-db/docs/api/vectors.md#replace_vector_row_dml--shared-delete-then-insert-replacement-546
fn replace_vector_row_dml(
    conn: &rusqlite::Connection,
    table: &str,
    dims: usize,
    row: VectorRowRef<'_>,
    record_ann_delta: bool,
    failpoint_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<(), rusqlite::Error> {
    if row.embedding.len() != dims {
        return Err(rusqlite::Error::InvalidParameterCount(
            row.embedding.len(),
            dims,
        ));
    }

    // Vector tables use subject_id as their primary key. Delete the common
    // same-identity row directly; its incoming upsert log is sufficient. Only
    // the metadata-repair path needs to discover and log the old ANN identity.
    // The caller's transaction/savepoint restores the prior row on failure.
    let subject_id = row.subject_id.to_string();
    let delete_same_identity_sql = format!(
        "DELETE FROM {table} WHERE subject_id = ?1 AND namespace = ?2 \
         AND embedding_model = ?3 AND kind = ?4 AND field = ?5"
    );
    let deleted_same_identity = conn.execute(
        &delete_same_identity_sql,
        rusqlite::params![
            &subject_id,
            row.namespace,
            row.embedding_model,
            row.kind,
            row.field
        ],
    )?;
    if deleted_same_identity == 0 {
        if record_ann_delta {
            let logged = log_vector_deletes(conn, table, "subject_id = ?1", &[&subject_id])?;
            if logged > 0 {
                let delete_prior_identity_sql =
                    format!("DELETE FROM {table} WHERE subject_id = ?1");
                conn.execute(&delete_prior_identity_sql, rusqlite::params![&subject_id])?;
            }
        } else {
            let delete_prior_identity_sql = format!("DELETE FROM {table} WHERE subject_id = ?1");
            conn.execute(&delete_prior_identity_sql, rusqlite::params![&subject_id])?;
        }
    }

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
            &subject_id,
            row.namespace,
            row.kind,
            row.field,
            row.embedding_model,
            blob
        ],
    )?;

    if provenance_sidecar_exists(conn)? {
        let model_key = table
            .strip_prefix("vec_")
            .expect("vector table names use the vec_ prefix");
        // Bind provenance to the bytes the live vec0 table exposes, rather than
        // assuming its read representation matches the input slice's layout.
        let stored_embedding: Vec<u8> = conn.query_row(
            &format!("SELECT embedding FROM {table} WHERE subject_id = ?1 AND namespace = ?2"),
            rusqlite::params![&subject_id, row.namespace],
            |stored| stored.get(0),
        )?;
        let embedding_digest = blake3::hash(&stored_embedding).to_hex().to_string();
        let updated_at = row.updated_at.map(DateTime::to_rfc3339);
        conn.execute(
            "INSERT INTO vector_provenance \
             (model_key, subject_id, namespace, embedding_digest, text_fingerprint, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(model_key, subject_id) DO UPDATE SET \
             namespace = excluded.namespace, \
             embedding_digest = excluded.embedding_digest, \
             text_fingerprint = excluded.text_fingerprint, \
             updated_at = excluded.updated_at",
            rusqlite::params![
                model_key,
                &subject_id,
                row.namespace,
                embedding_digest,
                row.text_fingerprint.map(ContentRef::as_str),
                updated_at,
            ],
        )?;
    }

    if record_ann_delta {
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
                &subject_id
            ],
        )?;
    }

    Ok(())
}

/// Log `'delete'` rows into `ann_write_log` for every vector row in `table`
/// matching `where_clause` (a predicate over the vec0 table's own columns).
/// Must run in the same transaction as — and before — the corresponding
/// `DELETE`, so the logged set is exactly the deleted set. Returns the number
/// of identities logged.
fn log_vector_deletes(
    conn: &rusqlite::Connection,
    table: &str,
    where_clause: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<usize, rusqlite::Error> {
    let sql = format!(
        "INSERT INTO ann_write_log (namespace, embedding_model, kind, field, subject_id, op) \
         SELECT namespace, embedding_model, kind, field, subject_id, 'delete' \
         FROM {table} WHERE {where_clause}"
    );
    conn.execute(&sql, params)
}

fn delete_vector_provenance(
    conn: &rusqlite::Connection,
    table: &str,
    subject_ids: &[String],
) -> Result<(), rusqlite::Error> {
    if subject_ids.is_empty() {
        return Ok(());
    }
    if !provenance_sidecar_exists(conn)? {
        return Ok(());
    }
    let model_key = table
        .strip_prefix("vec_")
        .expect("vector table names use the vec_ prefix");
    let placeholders = (2..=subject_ids.len() + 1)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "DELETE FROM vector_provenance \
         WHERE model_key = ?1 AND subject_id IN ({placeholders})"
    );
    let mut statement = conn.prepare(&sql)?;
    statement.raw_bind_parameter(1, model_key)?;
    for (index, subject_id) in subject_ids.iter().enumerate() {
        statement.raw_bind_parameter(index + 2, subject_id.as_str())?;
    }
    statement.raw_execute()?;
    Ok(())
}

/// DML-only multi-chunk subject deletion shared by both the legacy
/// (flag-off) and WriterTask-routed (flag-on) `delete_subjects` paths.
///
/// Issues no `BEGIN` / `COMMIT` / `ROLLBACK` / `SAVEPOINT`: the caller owns
/// one transaction around the complete input so a failure in any later chunk
/// rolls back every earlier vector deletion and matching ANN-log row.
fn delete_vector_subjects_dml(
    conn: &rusqlite::Connection,
    table: &str,
    id_strings: &[String],
) -> Result<u64, rusqlite::Error> {
    let mut total_deleted = 0u64;
    // vec0 only selects its point plan for primary-key equality outside KNN.
    let log_sql = format!(
        "INSERT INTO ann_write_log (namespace, embedding_model, kind, field, subject_id, op) \
         SELECT namespace, embedding_model, kind, field, subject_id, 'delete' \
         FROM {table} WHERE subject_id = ?1"
    );
    let mut log_stmt = conn.prepare(&log_sql)?;
    let mut delete_stmt = conn.prepare(&format!("DELETE FROM {table} WHERE subject_id = ?1"))?;

    // The relational provenance sidecar still uses bounded IN statements.
    for chunk in id_strings.chunks(400) {
        for id in chunk {
            log_stmt.execute([id.as_str()])?;
            total_deleted += delete_stmt.execute([id.as_str()])? as u64;
        }
        delete_vector_provenance(conn, table, chunk)?;
    }

    Ok(total_deleted)
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
    let subject_id = subject_id.to_string();
    for table in tables {
        log_vector_deletes(
            conn,
            table,
            "subject_id = ?1 AND namespace = ?2",
            &[&subject_id, &namespace],
        )?;
        let sql = format!("DELETE FROM {table} WHERE subject_id = ?1 AND namespace = ?2");
        if conn.execute(&sql, rusqlite::params![&subject_id, namespace])? > 0 {
            delete_vector_provenance(conn, table, std::slice::from_ref(&subject_id))?;
        }
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
    let mut summary = BatchWriteSummary {
        attempted,
        ..BatchWriteSummary::default()
    };

    for (index, record) in records.iter().enumerate() {
        let item_id = Some(record.subject_id.to_string());
        if record.vectors.len() != 1 {
            summary.record_failure(
                index,
                item_id,
                BatchWriteErrorClass::InvalidInput,
                BatchWriteRetryability::Permanent,
                format!("expected 1 vector per record, got {}", record.vectors.len()),
            );
            continue;
        }
        let embedding = &record.vectors[0];
        if embedding.len() != dims {
            summary.record_failure(
                index,
                item_id,
                BatchWriteErrorClass::InvalidInput,
                BatchWriteRetryability::Permanent,
                format!(
                    "wrong vector dimension: expected {dims}, got {}",
                    embedding.len()
                ),
            );
            continue;
        }
        if non_finite_index(embedding).is_some() {
            summary.record_failure(
                index,
                item_id,
                BatchWriteErrorClass::InvalidInput,
                BatchWriteRetryability::Permanent,
                "embedding contains non-finite values (NaN or Inf)",
            );
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
                text_fingerprint: record.text_fingerprint.as_ref(),
                updated_at: Some(&record.updated_at),
            },
            true,
            failpoint_flag.clone(),
        );
        match result {
            Ok(()) => {
                conn.execute_batch("RELEASE SAVEPOINT vec_batch_record")?;
                summary.affected = summary.affected.saturating_add(1);
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK TO SAVEPOINT vec_batch_record");
                let _ = conn.execute_batch("RELEASE SAVEPOINT vec_batch_record");
                let (class, retryability) = super::classify_batch_sqlite_error(&e);
                summary.record_failure(index, item_id, class, retryability, e.to_string());
            }
        }
    }

    Ok(summary)
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
    record_ann_delta: bool,
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
            text_fingerprint: None,
            updated_at: None,
        },
        record_ann_delta,
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
    // Policy-critical: only live subjects protect their vectors. Knowledge
    // atoms have their own core table but use Entity-kind vectors in this
    // same store. The table is required by the core schema: a missing table
    // must fail the sweep before deletion rather than treat atoms as absent.
    // Memory records live in `notes` with kind = 'memory'.
    let live_subq = "SELECT id FROM entities WHERE deleted_at IS NULL \
                     UNION ALL \
                     SELECT id FROM notes WHERE deleted_at IS NULL \
                     UNION ALL \
                     SELECT id FROM knowledge_atoms WHERE deleted_at IS NULL";

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

    // Snapshot the live registry once for this sweep. Each bounded delete
    // batch consults this temp table instead of rescanning all three source
    // tables. The caller's transaction owns the snapshot and any rollback.
    conn.execute_batch("CREATE TEMP TABLE khive_orphan_sweep_live_ids(id TEXT)")?;
    conn.execute_batch(&format!(
        "INSERT INTO temp.khive_orphan_sweep_live_ids(id) {live_subq}"
    ))?;
    conn.execute_batch(
        "CREATE INDEX temp.khive_orphan_sweep_live_ids_idx \
         ON khive_orphan_sweep_live_ids(id)",
    )?;
    let orphan_pred = format!(
        "subject_id NOT IN (SELECT id FROM temp.khive_orphan_sweep_live_ids) AND {filter_pred}"
    );

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
    // Select one bounded batch at a time. The same explicit IDs feed its log
    // insert and delete: evaluating a `LIMIT` subquery twice would not
    // guarantee that the logged and deleted rows are identical. The enclosing
    // writer transaction keeps later batches from racing another writer.
    let deleted: i64 = if dry_run {
        0
    } else {
        let select_sql = format!(
            "SELECT subject_id FROM {t} WHERE {p} LIMIT ?4",
            t = table,
            p = orphan_pred,
        );
        let mut select_stmt = conn.prepare(&select_sql)?;
        // vec0 only selects its point plan for primary-key equality outside KNN,
        // so the log insert and the delete each run once per victim id.
        let log_sql = format!(
            "INSERT INTO ann_write_log (namespace, embedding_model, kind, field, subject_id, op) \
             SELECT namespace, embedding_model, kind, field, subject_id, 'delete' \
             FROM {table} WHERE subject_id = ?1"
        );
        let mut log_stmt = conn.prepare(&log_sql)?;
        let del_sql = format!("DELETE FROM {table} WHERE subject_id = ?1");
        let mut delete_stmt = conn.prepare(&del_sql)?;
        let mut total: i64 = 0;
        let mut remaining = max_delete;
        while remaining > 0 {
            let batch_limit = remaining.min(400);
            let victim_ids: Vec<String> = select_stmt
                .query_map(
                    rusqlite::params![ns_json, kind_json, allow_json, batch_limit],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<Result<_, _>>()?;
            if victim_ids.is_empty() {
                break;
            }

            for id in &victim_ids {
                log_stmt.execute([id.as_str()])?;
                total += delete_stmt.execute([id.as_str()])? as i64;
            }
            // The relational provenance sidecar keeps its bounded IN statement.
            delete_vector_provenance(conn, table, &victim_ids)?;
            remaining -= victim_ids.len() as i64;
        }
        total
    };

    conn.execute_batch("DROP TABLE temp.khive_orphan_sweep_live_ids")?;

    Ok(OrphanSweepResult {
        scanned: scanned as u64,
        would_delete: would_delete as u64,
        deleted: deleted as u64,
        max_delete_hit,
    })
}

#[cfg(all(test, feature = "vectors"))]
#[path = "orphan_sweep_dml_tests.rs"]
mod orphan_sweep_dml_tests;

#[cfg(all(test, feature = "vectors"))]
#[path = "vector_read_tests.rs"]
mod vector_read_tests;

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
        self.insert_one(
            subject_id,
            kind,
            namespace,
            field,
            vectors,
            true,
            "vec_insert",
            "vec_insert_atomic",
        )
        .await
    }

    async fn insert_exact_only(
        &self,
        subject_id: Uuid,
        kind: SubstrateKind,
        namespace: &str,
        field: &str,
        vectors: Vec<Vec<f32>>,
    ) -> Result<(), StorageError> {
        self.insert_one(
            subject_id,
            kind,
            namespace,
            field,
            vectors,
            false,
            "vec_insert_exact_only",
            "vec_insert_exact_only_atomic",
        )
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
        if let Some(writer_task) = self.current_writer_task("vec_insert_batch")? {
            let table2 = table.clone();
            let store_embedding_model2 = store_embedding_model.clone();
            return writer_task
                .send_bounded(move |conn| {
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

        // The direct pooled fallback opens and admits one transaction before
        // this DML body, then settles it after the complete batch.
        self.with_writer("vec_insert_batch", move |conn| {
            batch_insert_vectors_dml(
                conn,
                &table,
                dims,
                &store_embedding_model,
                &records,
                attempted,
                failpoint_flag,
            )
        })
        .await
    }

    async fn provenance(&self, subject_id: Uuid) -> Result<Option<VectorProvenance>, StorageError> {
        let table = self.table_name.clone();
        let model_key = self.model_key.clone();
        let namespace = self.namespace.clone();
        self.with_reader("vec_provenance", move |conn| {
            let has_sidecar = provenance_sidecar_exists(conn)?;
            let sql = provenance_read_sql(&table, has_sidecar);
            let subject_id = subject_id.to_string();
            let with_sidecar: [&dyn rusqlite::ToSql; 3] = [&model_key, &subject_id, &namespace];
            let without_sidecar: [&dyn rusqlite::ToSql; 2] = [&subject_id, &namespace];
            let params: &[&dyn rusqlite::ToSql] = if has_sidecar {
                &with_sidecar
            } else {
                &without_sidecar
            };
            conn.query_row(&sql, params, |row| {
                let embedding_model = row.get(0)?;
                let field = row.get(1)?;
                let live_embedding: Vec<u8> = row.get(2)?;
                let stored_digest: Option<String> = row.get(3)?;
                let live_digest = blake3::hash(&live_embedding).to_hex().to_string();
                if stored_digest.as_deref() != Some(live_digest.as_str()) {
                    return Ok(VectorProvenance {
                        embedding_model,
                        field,
                        text_fingerprint: None,
                        updated_at: None,
                    });
                }
                let fingerprint: Option<String> = row.get(4)?;
                let text_fingerprint = fingerprint
                    .map(|raw| {
                        ContentRef::from_hex(raw).map_err(|message| {
                            rusqlite::Error::FromSqlConversionFailure(
                                4,
                                rusqlite::types::Type::Text,
                                Box::new(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    message,
                                )),
                            )
                        })
                    })
                    .transpose()?;
                let timestamp: Option<String> = row.get(5)?;
                let updated_at = timestamp
                    .map(|raw| {
                        DateTime::parse_from_rfc3339(&raw)
                            .map(|value| value.with_timezone(&Utc))
                            .map_err(|error| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    5,
                                    rusqlite::types::Type::Text,
                                    Box::new(error),
                                )
                            })
                    })
                    .transpose()?;
                Ok(VectorProvenance {
                    embedding_model,
                    field,
                    text_fingerprint,
                    updated_at,
                })
            })
            .optional()
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
        if let Some(writer_task) = self.current_writer_task("vec_update")? {
            let table2 = table.clone();
            let namespace2 = namespace.clone();
            let field2 = field.clone();
            let kind_str2 = kind_str.clone();
            let embedding_model2 = embedding_model.clone();
            let embedding2 = embedding.clone();
            return writer_task
                .send_bounded(move |conn| {
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
                        true,
                        failpoint_flag,
                    )
                    .map_err(|e| map_err(e, "vec_update"))
                })
                .await;
        }

        // The direct pooled fallback owns the admitted transaction. The
        // DELETE+INSERT body is shared with the WriterTask/batch paths (#546).
        self.with_writer("vec_update", move |conn| {
            replace_vector_row_dml(
                conn,
                &table,
                dims,
                VectorRowRef {
                    subject_id,
                    namespace: &namespace,
                    kind: &kind_str,
                    field: &field,
                    embedding_model: &embedding_model,
                    embedding: &embedding,
                    text_fingerprint: None,
                    updated_at: None,
                },
                true,
                failpoint_flag,
            )
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
                let deleted = stmt.raw_execute()? > 0;
                if deleted {
                    delete_vector_provenance(conn, &table, &[subject_id.to_string()])?;
                }
                Ok(deleted)
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

                hits.push(VectorSearchHit {
                    subject_id,
                    score: sqlite_cosine_score(distance)?,
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

        // The WriterTask owns one BEGIN IMMEDIATE/COMMIT/ROLLBACK around each
        // request. Submit the complete chunk loop as one DML-only request so a
        // failure in any chunk makes the task roll back the complete input.
        if let Some(writer_task) = self.current_writer_task("vec_delete_subjects")? {
            let table_for_error = table.clone();
            return writer_task
                .send_bounded(move |conn| {
                    delete_vector_subjects_dml(conn, &table, &id_strings)
                        .map_err(|e| map_err(e, "vec_delete_subjects"))
                })
                .await
                .map_err(|e| {
                    tracing::warn!(error = %e, table = %table_for_error, "delete_subjects failed");
                    e
                });
        }

        // The direct pooled path owns an admitted transaction around all
        // chunks and verifies rollback/autocommit before returning the writer.
        self.pool
            .record_direct_route(crate::timeout_sink::Site::DirectRouteVecDeleteSubjects);
        let table_for_error = table.clone();
        self.with_writer_unmanaged("vec_delete_subjects", move |conn| {
            delete_vector_subjects_dml(conn, &table, &id_strings)
        })
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, table = %table_for_error, "delete_subjects failed");
            e
        })
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
            // vec0's primary-key IN constraint otherwise selects a full scan.
            let sql = format!(
                "SELECT subject_id FROM {table} WHERE namespace = ?1 \
                 AND embedding_model = ?2 AND subject_id = ?3"
            );
            let mut stmt = conn.prepare(&sql)?;

            for id in id_strings {
                let id_str: Option<String> = stmt
                    .query_row(rusqlite::params![&namespace, &model, &id], |row| row.get(0))
                    .optional()?;
                if let Some(id_str) = id_str {
                    if let Ok(uuid) = Uuid::parse_str(&id_str) {
                        found.insert(uuid);
                    }
                }
            }

            Ok(found)
        })
        .await
    }

    async fn get_vectors(
        &self,
        ids: &[Uuid],
        namespace: &str,
        field: &str,
    ) -> StorageResult<std::collections::HashMap<Uuid, Vec<f32>>> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let table = self.table_name.clone();
        let namespace = namespace.to_owned();
        let field = field.to_owned();
        let model = self.embedding_model.clone();
        let dims = self.dimensions;
        let ids = ids.to_vec();

        self.with_reader("vec_get_vectors", move |conn| {
            // The vec0 subject_id primary key constrains each lookup before
            // metadata filtering, so the work is bounded by ids.len().
            let sql = format!(
                "SELECT embedding FROM {table} WHERE subject_id = ?1 \
                 AND namespace = ?2 AND field = ?3 AND embedding_model = ?4"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut found = std::collections::HashMap::with_capacity(ids.len());
            for id in ids {
                let blob: Option<Vec<u8>> = stmt
                    .query_row(
                        rusqlite::params![id.to_string(), &namespace, &field, &model],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(blob) = blob {
                    if blob.len() != dims * std::mem::size_of::<f32>() {
                        return Err(rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Blob,
                            Box::new(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!(
                                    "stored vector has {} bytes, expected {}",
                                    blob.len(),
                                    dims * std::mem::size_of::<f32>()
                                ),
                            )),
                        ));
                    }
                    let vector = blob
                        .chunks_exact(std::mem::size_of::<f32>())
                        // Inserts bind f32_slice_as_bytes, which writes native-endian
                        // f32 bytes. Decode with the same layout on every target.
                        .map(|bytes| f32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                        .collect();
                    found.insert(id, vector);
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
        if let Some(writer_task) = self.current_writer_task("orphan_sweep")? {
            let table2 = table.clone();
            let ns_json2 = ns_json.clone();
            let kind_json2 = kind_json.clone();
            let allow_json2 = allow_json.clone();
            return writer_task
                .send_bounded(move |conn| {
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

        // The direct pooled fallback owns the admitted transaction around
        // this DML body and verifies rollback/autocommit on every outcome.
        self.pool
            .record_direct_route(crate::timeout_sink::Site::DirectRouteOrphanSweep);
        self.with_writer_unmanaged("orphan_sweep", move |conn| {
            orphan_sweep_dml(
                conn,
                &table,
                ns_json.as_deref(),
                kind_json.as_deref(),
                allow_json.as_deref(),
                max_delete,
                dry_run,
            )
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
            supports_vector_read: true,
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

        if candidate_ids.is_empty() {
            return Ok(Vec::new());
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
            let sql = format!(
                "SELECT e.subject_id, vec_distance_cosine(e.embedding, ?1) as distance \
                 FROM {table} e \
                 WHERE e.namespace = ?2 AND e.embedding_model = ?3 \
                   AND e.subject_id = ?4"
            );
            let mut stmt = conn.prepare(&sql)?;

            // Preserve IN's duplicate suppression within each original group,
            // including repeated hits for IDs supplied in different groups.
            for chunk in ids.chunks(399) {
                let mut seen = HashSet::with_capacity(chunk.len());
                for id in chunk.iter().filter(|id| seen.insert(*id)) {
                    let row: Option<(String, f64)> = stmt
                        .query_row(
                            rusqlite::params![query_blob, &namespace, &embedding_model, id],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()?;
                    let Some((id_str, distance)) = row else {
                        continue;
                    };

                    let subject_id = Uuid::parse_str(&id_str).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?;

                    all_hits.push(VectorSearchHit {
                        subject_id,
                        score: sqlite_cosine_score(distance)?,
                        rank: 0,
                    });
                }
            }

            all_hits
                .sort_by(|a, b| cmp_desc_then_id(a.score, &a.subject_id, b.score, &b.subject_id));
            for (i, hit) in all_hits.iter_mut().enumerate() {
                hit.rank = (i + 1) as u32;
            }

            Ok(all_hits)
        })
        .await
    }
}

#[cfg(all(test, feature = "vectors"))]
mod point_lookup_tests {
    use std::collections::HashMap;

    use super::*;
    use crate::pool::PoolConfig;

    const MODEL: &str = "point_lookup";
    const NAMESPACE: &str = "ns:point_lookup";
    const TABLE: &str = "vec_point_lookup";

    struct StoredVector {
        id: Uuid,
        namespace: &'static str,
        model: &'static str,
        kind: &'static str,
        field: &'static str,
        embedding: [f32; 2],
    }

    struct Fixture {
        pool: Arc<ConnectionPool>,
        store: SqliteVecStore,
        requested: Vec<Uuid>,
        stored: Vec<StoredVector>,
    }

    impl Fixture {
        fn new() -> Self {
            crate::extension::ensure_extensions_loaded();
            let pool = Arc::new(
                ConnectionPool::new(PoolConfig {
                    path: None,
                    write_queue_enabled: Some(false),
                    ..PoolConfig::for_test()
                })
                .expect("in-memory pool"),
            );
            let store = SqliteVecStore::new(
                Arc::clone(&pool),
                false,
                MODEL.into(),
                MODEL.into(),
                2,
                NAMESPACE.into(),
            )
            .expect("vector store");
            let mut requested: Vec<Uuid> = (1..=805).map(Uuid::from_u128).collect();
            // Exercise duplicates both within and across the original 399-ID groups.
            requested[7] = requested[2];
            requested[401] = requested[2];
            let stored: Vec<StoredVector> = (0..806)
                .filter(|index| index % 4 != 0)
                .map(|index| StoredVector {
                    id: Uuid::from_u128(index + 1),
                    namespace: if index % 5 == 0 {
                        "ns:other"
                    } else {
                        NAMESPACE
                    },
                    model: if index % 7 == 0 { "other_model" } else { MODEL },
                    kind: if index % 2 == 0 { "entity" } else { "note" },
                    field: if index % 2 == 0 { "body" } else { "title" },
                    embedding: match index % 3 {
                        0 => [1.0, 0.0],
                        1 => [0.0, 1.0],
                        _ => [-1.0, 0.0],
                    },
                })
                .collect();
            {
                let writer = pool.try_writer().expect("pool writer");
                let conn = writer.conn();
                conn.execute_batch(
                    "CREATE VIRTUAL TABLE vec_point_lookup USING vec0(\
                     subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, \
                     kind TEXT NOT NULL, field TEXT NOT NULL, embedding_model TEXT NOT NULL, \
                     embedding float[2] distance_metric=cosine)",
                )
                .expect("create vector table");
                conn.execute_batch(crate::migrations::ANN_WRITE_LOG_DDL)
                    .expect("create write log");
                conn.execute_batch(crate::migrations::VECTOR_PROVENANCE_DDL)
                    .expect("create provenance sidecar");
                let mut insert = conn
                    .prepare(
                        "INSERT INTO vec_point_lookup \
                         (subject_id, namespace, embedding_model, kind, field, embedding) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    )
                    .expect("prepare seed insert");
                let mut provenance = conn
                    .prepare(
                        "INSERT INTO vector_provenance \
                         (model_key, subject_id, namespace, embedding_digest) \
                         VALUES (?1, ?2, ?3, ?4)",
                    )
                    .expect("prepare seed provenance");
                for vector in &stored {
                    insert
                        .execute(rusqlite::params![
                            vector.id.to_string(),
                            vector.namespace,
                            vector.model,
                            vector.kind,
                            vector.field,
                            f32_slice_as_bytes(&vector.embedding),
                        ])
                        .expect("seed vector");
                    provenance
                        .execute(rusqlite::params![
                            MODEL,
                            vector.id.to_string(),
                            vector.namespace,
                            blake3::hash(f32_slice_as_bytes(&vector.embedding))
                                .to_hex()
                                .to_string(),
                        ])
                        .expect("seed provenance");
                }
            }
            Self {
                pool,
                store,
                requested,
                stored,
            }
        }

        fn expected_exists(&self) -> HashSet<Uuid> {
            let requested: HashSet<Uuid> = self.requested.iter().copied().collect();
            self.stored
                .iter()
                .filter(|vector| {
                    requested.contains(&vector.id)
                        && vector.namespace == NAMESPACE
                        && vector.model == MODEL
                })
                .map(|vector| vector.id)
                .collect()
        }

        fn expected_scores(&self) -> Vec<(Uuid, DeterministicScore, u32)> {
            let stored: HashMap<_, _> = self.stored.iter().map(|v| (v.id, v)).collect();
            let mut expected = Vec::new();
            for chunk in self.requested.chunks(399) {
                let mut seen = HashSet::new();
                for id in chunk.iter().filter(|id| seen.insert(*id)) {
                    if let Some(vector) = stored.get(id) {
                        if vector.namespace == NAMESPACE && vector.model == MODEL {
                            expected.push((
                                *id,
                                DeterministicScore::from_f32(vector.embedding[0]),
                                0,
                            ));
                        }
                    }
                }
            }
            expected.sort_by(|a, b| cmp_desc_then_id(a.1, &a.0, b.1, &b.0));
            for (index, hit) in expected.iter_mut().enumerate() {
                hit.2 = (index + 1) as u32;
            }
            expected
        }
    }

    // Capture statements executed by the public operations, so planner checks
    // cannot accidentally exercise a separate copy of their SQL. This rides the
    // pool's shared statement observer: SQLite keeps one trace callback per
    // connection, so a second hook here would silence the observer.
    struct StatementCapture {
        observation: crate::StatementStartObservation,
    }

    impl StatementCapture {
        fn new(pool: Arc<ConnectionPool>) -> Self {
            // The bound turns an unexpectedly long run into an error rather
            // than a silently short list.
            let observation = pool
                .observe_test_statement_starts(100_000)
                .expect("observe statement starts");
            Self { observation }
        }

        fn finish(self) -> Vec<String> {
            self.observation
                .started_statements()
                .expect("captured statements")
                .into_iter()
                .map(|statement| statement.sql)
                .collect()
        }
    }

    fn assert_point_plans(pool: &ConnectionPool, statements: &[String], prefix: &str) {
        let statements: HashSet<_> = statements
            .iter()
            .filter(|sql| sql.starts_with(prefix) && sql.contains(TABLE))
            .collect();
        assert!(
            !statements.is_empty(),
            "production statement was not captured: {prefix}"
        );
        let writer = pool.try_writer().expect("pool writer");
        for sql in statements {
            let mut explain = writer
                .conn()
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .expect("explain production statement");
            let mut rows = explain.raw_query();
            let mut virtual_plans = Vec::new();
            while let Some(row) = rows.next().expect("query plan row") {
                let detail: String = row.get(3).expect("query plan detail");
                if detail.contains("VIRTUAL TABLE INDEX ") {
                    virtual_plans.push(detail);
                }
            }
            assert_eq!(virtual_plans.len(), 1, "expected one vec0 plan for {sql}");
            // sqlite-vec 0.1.9 uses idxStr '2!___' for its POINT plan and '1'
            // for FULLSCAN. idxNum depends on the projected columns.
            assert!(
                virtual_plans[0]
                    .rsplit_once(':')
                    .is_some_and(|(_, index)| index.starts_with("2!")),
                "expected vec0 POINT plan for {sql}, got {virtual_plans:?}"
            );
        }
    }

    #[tokio::test]
    async fn batch_exists_uses_point_plan_for_mixed_ids_across_chunks() {
        let fixture = Fixture::new();
        let capture = StatementCapture::new(Arc::clone(&fixture.pool));
        let found = fixture
            .store
            .batch_exists(&fixture.requested, NAMESPACE)
            .await
            .expect("batch existence check");
        let statements = capture.finish();
        assert_eq!(found, fixture.expected_exists());
        assert_point_plans(&fixture.pool, &statements, "SELECT subject_id FROM ");
    }

    #[tokio::test]
    async fn score_candidates_uses_point_plan_for_mixed_ids_across_chunks() {
        let fixture = Fixture::new();
        let capture = StatementCapture::new(Arc::clone(&fixture.pool));
        let hits = fixture
            .store
            .score_candidates(&[1.0, 0.0], &fixture.requested)
            .await
            .expect("score candidates");
        let statements = capture.finish();
        let actual: Vec<_> = hits
            .iter()
            .map(|hit| (hit.subject_id, hit.score, hit.rank))
            .collect();
        assert_eq!(actual, fixture.expected_scores());
        assert_point_plans(&fixture.pool, &statements, "SELECT e.subject_id, ");
    }

    #[tokio::test]
    async fn delete_subjects_uses_point_plans_for_mixed_ids_across_chunks() {
        let fixture = Fixture::new();
        let requested: HashSet<_> = fixture.requested.iter().copied().collect();
        let mut expected_log: Vec<_> = fixture
            .stored
            .iter()
            .filter(|vector| requested.contains(&vector.id))
            .map(|vector| {
                (
                    vector.namespace.to_string(),
                    vector.model.to_string(),
                    vector.kind.to_string(),
                    vector.field.to_string(),
                    vector.id.to_string(),
                    "delete".to_string(),
                )
            })
            .collect();
        expected_log.sort();
        let expected_remaining: HashSet<_> = fixture
            .stored
            .iter()
            .filter(|vector| !requested.contains(&vector.id))
            .map(|vector| vector.id.to_string())
            .collect();
        assert!(
            !expected_remaining.is_empty(),
            "fixture must retain unrequested vectors"
        );
        let capture = StatementCapture::new(Arc::clone(&fixture.pool));
        let deleted = fixture
            .store
            .delete_subjects(&fixture.requested)
            .await
            .expect("delete subjects");
        let statements = capture.finish();
        assert_eq!(deleted as usize, expected_log.len());
        {
            let writer = fixture.pool.try_writer().expect("pool writer");
            let conn = writer.conn();
            let mut log = conn.prepare(
                "SELECT namespace, embedding_model, kind, field, subject_id, op FROM ann_write_log"
            ).expect("read delete log");
            let mut actual_log: Vec<(String, String, String, String, String, String)> = log
                .query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                })
                .expect("query delete log")
                .collect::<Result<_, _>>()
                .expect("delete log rows");
            actual_log.sort();
            assert_eq!(actual_log, expected_log);
            for sql in [
                "SELECT subject_id FROM vec_point_lookup",
                "SELECT subject_id FROM vector_provenance WHERE model_key = 'point_lookup'",
            ] {
                let actual: HashSet<String> = conn
                    .prepare(sql)
                    .expect("read surviving identities")
                    .query_map([], |row| row.get(0))
                    .expect("query surviving identities")
                    .collect::<Result<_, _>>()
                    .expect("surviving identities");
                assert_eq!(actual, expected_remaining);
            }
            assert!(conn.is_autocommit(), "delete must finish its transaction");
        }
        assert_point_plans(&fixture.pool, &statements, "INSERT INTO ann_write_log ");
        assert_point_plans(&fixture.pool, &statements, "DELETE FROM vec_point_lookup ");
    }

    #[tokio::test]
    async fn orphan_sweep_uses_point_plans_across_batches() {
        let fixture = Fixture::new();
        // Every ninth stored vector keeps a live entity. The rest are orphans,
        // enough for one full 400-victim batch and a partial one.
        let live: HashSet<String> = fixture
            .stored
            .iter()
            .enumerate()
            .filter(|(index, _)| index % 9 == 0)
            .map(|(_, vector)| vector.id.to_string())
            .collect();
        {
            let writer = fixture.pool.try_writer().expect("pool writer");
            let conn = writer.conn();
            conn.execute_batch(
                "CREATE TABLE entities (id TEXT PRIMARY KEY, deleted_at INTEGER); \
                 CREATE TABLE notes (id TEXT PRIMARY KEY, deleted_at INTEGER); \
                 CREATE TABLE knowledge_atoms (id TEXT PRIMARY KEY, deleted_at INTEGER)",
            )
            .expect("create live-subject tables");
            let mut insert = conn
                .prepare("INSERT INTO entities (id, deleted_at) VALUES (?1, NULL)")
                .expect("prepare live entity insert");
            for id in &live {
                insert.execute([id.as_str()]).expect("seed live entity");
            }
        }
        let mut expected_log: Vec<_> = fixture
            .stored
            .iter()
            .filter(|vector| !live.contains(&vector.id.to_string()))
            .map(|vector| {
                (
                    vector.namespace.to_string(),
                    vector.model.to_string(),
                    vector.kind.to_string(),
                    vector.field.to_string(),
                    vector.id.to_string(),
                    "delete".to_string(),
                )
            })
            .collect();
        expected_log.sort();
        assert!(
            expected_log.len() > 400,
            "fixture must need more than one delete batch"
        );
        let capture = StatementCapture::new(Arc::clone(&fixture.pool));
        let result = fixture
            .store
            .orphan_sweep(&OrphanSweepConfig {
                subject_id_allowlist: None,
                namespaces: vec![],
                substrate_kinds: vec![],
                max_delete: 1000,
                dry_run: false,
            })
            .await
            .expect("orphan sweep");
        let statements = capture.finish();
        assert_eq!(result.scanned as usize, fixture.stored.len());
        assert_eq!(result.would_delete as usize, expected_log.len());
        assert_eq!(result.deleted as usize, expected_log.len());
        assert!(!result.max_delete_hit);
        {
            let writer = fixture.pool.try_writer().expect("pool writer");
            let conn = writer.conn();
            let mut log = conn
                .prepare(
                    "SELECT namespace, embedding_model, kind, field, subject_id, op \
                     FROM ann_write_log",
                )
                .expect("read delete log");
            let mut actual_log: Vec<(String, String, String, String, String, String)> = log
                .query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                })
                .expect("query delete log")
                .collect::<Result<_, _>>()
                .expect("delete log rows");
            actual_log.sort();
            assert_eq!(actual_log, expected_log);
            for sql in [
                "SELECT subject_id FROM vec_point_lookup",
                "SELECT subject_id FROM vector_provenance WHERE model_key = 'point_lookup'",
            ] {
                let actual: HashSet<String> = conn
                    .prepare(sql)
                    .expect("read surviving identities")
                    .query_map([], |row| row.get(0))
                    .expect("query surviving identities")
                    .collect::<Result<_, _>>()
                    .expect("surviving identities");
                assert_eq!(actual, live);
            }
            assert!(conn.is_autocommit(), "sweep must finish its transaction");
        }
        assert_point_plans(&fixture.pool, &statements, "INSERT INTO ann_write_log ");
        assert_point_plans(&fixture.pool, &statements, "DELETE FROM vec_point_lookup ");
    }

    #[tokio::test]
    async fn observer_still_records_after_a_statement_capture_is_dropped() {
        let fixture = Fixture::new();
        drop(StatementCapture::new(Arc::clone(&fixture.pool)));
        let observation = fixture
            .pool
            .observe_test_statement_starts(8)
            .expect("observe after capture");
        let sql = "SELECT 17 AS after_capture";
        {
            let writer = fixture.pool.try_writer().expect("pool writer");
            let value: i64 = writer
                .conn()
                .query_row(sql, [], |row| row.get(0))
                .expect("run statement");
            assert_eq!(value, 17);
        }
        assert_eq!(
            observation.started_statements().expect("observed starts"),
            vec![crate::StartedStatement {
                sql: sql.to_owned(),
                readonly: true,
            }],
            "a dropped capture must leave the shared observer recording"
        );
    }
}

#[cfg(test)]
mod unmanaged_write_escalation_tests;

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
            .execute_batch(crate::migrations::VECTOR_PROVENANCE_DDL)
            .expect("create vector_provenance");
        writer
            .conn()
            .execute_batch(crate::migrations::ANN_WRITE_LOG_DDL)
            .expect("create ann_write_log");
    }

    #[tokio::test]
    async fn sqlite_vector_paths_use_canonical_opposite_cosine_score() {
        let pool = make_vec_pool();
        let model_key = "canonical_cosine_score";
        let namespace = "ns:canonical-score";
        create_vec_table(&pool, model_key, 2);
        let store = SqliteVecStore::new(
            pool,
            false,
            model_key.to_string(),
            model_key.to_string(),
            2,
            namespace.to_string(),
        )
        .unwrap();
        let opposite_id = Uuid::from_u128(1);
        store
            .insert(
                opposite_id,
                SubstrateKind::Entity,
                namespace,
                "body",
                vec![vec![-1.0, 0.0]],
            )
            .await
            .unwrap();

        let candidate_hits = store
            .score_candidates(&[1.0, 0.0], &[opposite_id])
            .await
            .unwrap();
        assert_eq!(candidate_hits.len(), 1);
        assert_eq!(candidate_hits[0].score, DeterministicScore::from_f64(-1.0));

        let search_hits = store
            .search(VectorSearchRequest {
                query_vectors: vec![vec![1.0, 0.0]],
                top_k: 1,
                namespace: Some(namespace.to_string()),
                kind: Some(SubstrateKind::Entity),
                embedding_model: None,
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        assert_eq!(search_hits.len(), 1);
        assert_eq!(search_hits[0].score, DeterministicScore::from_f64(-1.0));
    }

    #[tokio::test]
    async fn exact_only_insert_replaces_rows_without_ann_deltas() {
        let pool = make_vec_pool();
        let raw_pool = Arc::clone(&pool);
        let model_key = "exact_only_no_ann_delta";
        let namespace = "ns:exact-only";
        create_vec_table(&pool, model_key, 2);
        let store = SqliteVecStore::new(
            pool,
            false,
            model_key.to_string(),
            model_key.to_string(),
            2,
            namespace.to_string(),
        )
        .unwrap();
        let exact_id = Uuid::from_u128(1);

        store
            .insert_exact_only(
                exact_id,
                SubstrateKind::Entity,
                namespace,
                "visual.descriptor",
                vec![vec![1.0, 0.0]],
            )
            .await
            .unwrap();
        store
            .insert_exact_only(
                exact_id,
                SubstrateKind::Entity,
                "ns:exact-only-repaired",
                "visual.descriptor",
                vec![vec![0.0, 1.0]],
            )
            .await
            .unwrap();

        let writer = raw_pool.try_writer().expect("pool writer");
        let deltas: i64 = writer
            .conn()
            .query_row("SELECT COUNT(*) FROM ann_write_log", [], |row| row.get(0))
            .unwrap();
        assert_eq!(deltas, 0);
        let repaired_namespace: String = writer
            .conn()
            .query_row(
                &format!("SELECT namespace FROM vec_{model_key} WHERE subject_id = ?1"),
                [exact_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(repaired_namespace, "ns:exact-only-repaired");
        drop(writer);

        store
            .insert(
                Uuid::from_u128(2),
                SubstrateKind::Entity,
                namespace,
                "body",
                vec![vec![1.0, 0.0]],
            )
            .await
            .unwrap();
        let writer = raw_pool.try_writer().expect("pool writer");
        let deltas: i64 = writer
            .conn()
            .query_row("SELECT COUNT(*) FROM ann_write_log", [], |row| row.get(0))
            .unwrap();
        assert_eq!(deltas, 1, "generic inserts must retain ANN logging");
    }

    /// sqlite-vec's own `distance_cosine_float` (sqlite-vec.c) accumulates
    /// `dot`/`aMag`/`bMag` in f32, so a non-axis-aligned vector's self- and
    /// opposite-comparison lands a few f32 ULPs off the exact 0/2 endpoint —
    /// not the tighter f64 ULP an in-process helper call would produce. This
    /// drives the real sqlite-vec extension end to end (not just the
    /// conversion helper) so the endpoint tolerance is checked against the
    /// roundoff sqlite-vec actually returns.
    #[tokio::test]
    async fn sqlite_vector_paths_tolerate_real_f32_endpoint_roundoff() {
        let pool = make_vec_pool();
        let model_key = "f32_endpoint_roundoff";
        let namespace = "ns:f32-roundoff";
        create_vec_table(&pool, model_key, 3);
        let store = SqliteVecStore::new(
            pool,
            false,
            model_key.to_string(),
            model_key.to_string(),
            3,
            namespace.to_string(),
        )
        .unwrap();

        let identical_id = Uuid::from_u128(1);
        let opposite_id = Uuid::from_u128(2);
        store
            .insert(
                identical_id,
                SubstrateKind::Entity,
                namespace,
                "body",
                vec![vec![0.1, 0.2, 0.3]],
            )
            .await
            .unwrap();
        store
            .insert(
                opposite_id,
                SubstrateKind::Entity,
                namespace,
                "body",
                vec![vec![-0.1, -0.2, -0.3]],
            )
            .await
            .unwrap();

        let candidate_hits = store
            .score_candidates(&[0.1, 0.2, 0.3], &[identical_id, opposite_id])
            .await
            .unwrap();
        let identical_score = candidate_hits
            .iter()
            .find(|hit| hit.subject_id == identical_id)
            .expect("identical candidate scored")
            .score;
        let opposite_score = candidate_hits
            .iter()
            .find(|hit| hit.subject_id == opposite_id)
            .expect("opposite candidate scored")
            .score;
        assert_eq!(identical_score, DeterministicScore::from_f64(1.0));
        assert_eq!(opposite_score, DeterministicScore::from_f64(-1.0));

        let search_hits = store
            .search(VectorSearchRequest {
                query_vectors: vec![vec![0.1, 0.2, 0.3]],
                top_k: 2,
                namespace: Some(namespace.to_string()),
                kind: Some(SubstrateKind::Entity),
                embedding_model: None,
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        assert_eq!(search_hits.len(), 2);
        let top_hit = search_hits
            .iter()
            .find(|hit| hit.subject_id == identical_id)
            .expect("identical vector present in search results");
        assert_eq!(top_hit.score, DeterministicScore::from_f64(1.0));
        assert_eq!(top_hit.rank, 1);
    }

    /// Derivation (checked against sqlite-vec's `distance_cosine_float` in
    /// sqlite-vec.c, which accumulates `dot`/`aMag`/`bMag` in f32 but calls
    /// the C `sqrt` — not `sqrtf` — on those f32 accumulators, so the
    /// square-root, product, and division all run in `double` before the
    /// final implicit narrowing back to `f32` on return).
    ///
    /// Because the widening to `double` happens before the square root, a
    /// self- or exactly-proportional comparison (`dot == aMag == bMag`
    /// bit-for-bit in f32) round-trips through `sqrt` at `double` precision
    /// and lands within `f64::EPSILON` of the exact endpoint — nowhere near
    /// the `f32::EPSILON` scale. The f32-scale roundoff this driver actually
    /// exposes instead comes from the *accumulation* step: `dot` and the two
    /// magnitudes are summed independently, so an exactly anti-parallel pair
    /// (`query = -1.5 * stored`, mathematical cosine `-1`, ideal distance
    /// `2`) can still see `dot` round to a f32 value fractionally larger in
    /// magnitude than `sqrt(aMag) * sqrt(bMag)` would predict.
    ///
    /// For `stored = [-4.8, -0.4, -0.4]` and `query = [7.2, 0.6, 0.6]`, that
    /// yields a raw distance of `2.0000002384185791` — exactly
    /// `2 + 2*f32::EPSILON`. Verified against the vendored
    /// `distance_cosine_float` body compiled standalone under every
    /// combination of `-O0`/`-O2`/`-O3` and `-ffp-contract=off`/`fast` plus
    /// explicit `-mavx2 -mfma`, since whether the target architecture fuses
    /// the per-term multiply-add changes the *path* to this result but not
    /// the final f32 value — the fixture is architecture-independent. That
    /// distance is outside the mathematical `[0, 2]` cosine-distance range,
    /// yet its magnitude is within the widened `8*f32::EPSILON` boundary
    /// (~9.537e-7) and enormously outside the old `4*f64::EPSILON`
    /// tolerance (~8.88e-16) that round one shipped. A driver that still
    /// used the old f64-scale window would reject this distance outright.
    #[tokio::test]
    async fn sqlite_vector_paths_tolerate_real_f32_endpoint_roundoff_above_two() {
        let pool = make_vec_pool();
        let raw_pool = Arc::clone(&pool);
        let model_key = "f32_endpoint_roundoff_above_two";
        let namespace = "ns:f32-roundoff-above-two";
        create_vec_table(&pool, model_key, 3);
        let store = SqliteVecStore::new(
            pool,
            false,
            model_key.to_string(),
            model_key.to_string(),
            3,
            namespace.to_string(),
        )
        .unwrap();

        let stored_id = Uuid::from_u128(1);
        let stored_vector = vec![-4.8_f32, -0.4, -0.4];
        let query_vector = vec![7.2_f32, 0.6, 0.6];
        store
            .insert(
                stored_id,
                SubstrateKind::Entity,
                namespace,
                "body",
                vec![stored_vector],
            )
            .await
            .unwrap();

        let raw_distance: f64 = {
            let writer = raw_pool.try_writer().expect("pool writer");
            let sql = format!(
                "SELECT vec_distance_cosine(embedding, ?1) FROM vec_{} WHERE subject_id = ?2",
                model_key
            );
            let mut stmt = writer.conn().prepare(&sql).unwrap();
            stmt.raw_bind_parameter(1, f32_slice_as_bytes(&query_vector))
                .unwrap();
            stmt.raw_bind_parameter(2, stored_id.to_string().as_str())
                .unwrap();
            let mut rows = stmt.raw_query();
            let row = rows.next().unwrap().expect("one row");
            row.get(0).unwrap()
        };

        assert_eq!(raw_distance, 2.0 + 2.0 * f32::EPSILON as f64);
        assert!(
            !(0.0..=2.0).contains(&raw_distance),
            "fixture must land outside the mathematical [0, 2] cosine range, got {raw_distance}"
        );
        let deviation_above_two = raw_distance - 2.0;
        let boundary_epsilon = 8.0 * f32::EPSILON as f64;
        assert!(
            deviation_above_two <= boundary_epsilon,
            "fixture must stay within the widened f32-scale tolerance, got {raw_distance}"
        );
        let old_tolerance = 4.0 * f64::EPSILON;
        assert!(
            deviation_above_two > old_tolerance,
            "fixture must exceed the old f64-scale tolerance so it would have failed under it, got {raw_distance}"
        );

        let candidate_hits = store
            .score_candidates(&query_vector, &[stored_id])
            .await
            .unwrap();
        assert_eq!(candidate_hits.len(), 1);
        assert_eq!(candidate_hits[0].score, DeterministicScore::from_f64(-1.0));

        let search_hits = store
            .search(VectorSearchRequest {
                query_vectors: vec![query_vector],
                top_k: 1,
                namespace: Some(namespace.to_string()),
                kind: Some(SubstrateKind::Entity),
                embedding_model: None,
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        assert_eq!(search_hits.len(), 1);
        assert_eq!(search_hits[0].score, DeterministicScore::from_f64(-1.0));
        assert_eq!(search_hits[0].rank, 1);
    }

    #[tokio::test]
    async fn score_candidates_rejects_an_empty_query_dimension() {
        let pool = make_vec_pool();
        let store = SqliteVecStore::new(
            pool,
            false,
            "empty_query_dimension".to_string(),
            "empty_query_dimension".to_string(),
            2,
            "ns:empty-query".to_string(),
        )
        .unwrap();

        let error = store
            .score_candidates(&[], &[Uuid::from_u128(1)])
            .await
            .unwrap_err();
        match error {
            StorageError::InvalidInput {
                operation, message, ..
            } => {
                assert_eq!(operation, "score_candidates");
                assert!(message.contains("query has 0 dims, expected 2"));
            }
            other => panic!("expected dimension error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn score_candidates_breaks_equal_scores_by_lower_id() {
        let pool = make_vec_pool();
        let model_key = "candidate_score_ties";
        let namespace = "ns:candidate-ties";
        create_vec_table(&pool, model_key, 2);
        let store = SqliteVecStore::new(
            pool,
            false,
            model_key.to_string(),
            model_key.to_string(),
            2,
            namespace.to_string(),
        )
        .unwrap();
        let lower_id = Uuid::from_u128(1);
        let higher_id = Uuid::from_u128(2);
        for id in [higher_id, lower_id] {
            store
                .insert(
                    id,
                    SubstrateKind::Entity,
                    namespace,
                    "body",
                    vec![vec![1.0, 0.0]],
                )
                .await
                .unwrap();
        }

        let hits = store
            .score_candidates(&[1.0, 0.0], &[higher_id, lower_id])
            .await
            .unwrap();
        assert_eq!(
            hits.iter().map(|hit| hit.subject_id).collect::<Vec<_>>(),
            vec![lower_id, higher_id]
        );
        assert_eq!(
            hits.iter().map(|hit| hit.rank).collect::<Vec<_>>(),
            vec![1, 2]
        );
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
mod first_error_tests;

#[cfg(test)]
mod capabilities_tests;

#[cfg(all(test, feature = "vectors"))]
mod delete_subjects_atomic_tests;

#[cfg(all(test, feature = "vectors"))]
mod atomic_replace_tests;

// ---------------------------------------------------------------------------
// Orphan sweep tests
// ---------------------------------------------------------------------------
// Require the `vectors` feature because the sweep queries the vec0 virtual
// table, which only exists when the sqlite-vec extension is loaded.
// ---------------------------------------------------------------------------
#[cfg(all(test, feature = "vectors"))]
mod orphan_sweep_tests;

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
mod write_queue_tests;

#[cfg(all(test, feature = "vectors"))]
mod provenance_tests {
    use std::sync::Arc;

    use super::*;
    use crate::pool::{ConnectionPool, PoolConfig};

    fn make_store() -> (Arc<ConnectionPool>, SqliteVecStore) {
        crate::extension::ensure_extensions_loaded();
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: None,
                ..PoolConfig::default()
            })
            .expect("in-memory pool"),
        );
        {
            let writer = pool.try_writer().expect("writer");
            writer
                .conn()
                .execute_batch(
                    "CREATE VIRTUAL TABLE vec_provenance_test USING vec0(\
                     subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, \
                     kind TEXT NOT NULL, field TEXT NOT NULL, \
                     embedding_model TEXT NOT NULL, embedding float[2] distance_metric=cosine)",
                )
                .expect("create vec0 table");
            writer
                .conn()
                .execute_batch(crate::migrations::ANN_WRITE_LOG_DDL)
                .expect("create ANN log");
            writer
                .conn()
                .execute_batch(crate::migrations::VECTOR_PROVENANCE_DDL)
                .expect("create provenance table");
        }
        let store = SqliteVecStore::new(
            Arc::clone(&pool),
            false,
            "provenance_test".into(),
            "model/a".into(),
            2,
            "ns:test".into(),
        )
        .expect("vector store");
        (pool, store)
    }

    fn record(subject_id: Uuid, text: &str, updated_at: &DateTime<Utc>) -> VectorRecord {
        VectorRecord {
            subject_id,
            kind: SubstrateKind::Entity,
            namespace: "ns:test".into(),
            field: "entity.body".into(),
            embedding_model: Some("model/a".into()),
            vectors: vec![vec![0.1, 0.2]],
            text_fingerprint: Some(VectorRecord::fingerprint_text(text)),
            updated_at: *updated_at,
        }
    }

    fn joined_provenance_count(
        conn: &rusqlite::Connection,
        table: &str,
        model_key: &str,
        subject_id: Uuid,
        namespace: &str,
    ) -> i64 {
        let sql = format!(
            "SELECT COUNT(embedding_digest) FROM ({})",
            provenance_read_sql(table, true)
        );
        conn.query_row(
            &sql,
            rusqlite::params![model_key, subject_id.to_string(), namespace],
            |row| row.get(0),
        )
        .expect("count sidecar matches through production read join")
    }

    #[tokio::test]
    async fn persisted_vector_provenance_tracks_exact_embedded_text() {
        let (pool, store) = make_store();
        let subject_id = Uuid::new_v4();
        let timestamp = DateTime::parse_from_rfc3339("2026-09-25T12:34:56Z")
            .unwrap()
            .with_timezone(&Utc);
        let original_text = "rendered title\nbody";
        store
            .insert_batch(vec![record(subject_id, original_text, &timestamp)])
            .await
            .expect("index original text");
        let expected =
            ContentRef::from_digest_bytes(blake3::hash(original_text.as_bytes()).as_bytes());
        let stored = store.provenance(subject_id).await.unwrap().unwrap();
        assert_eq!(stored.embedding_model, "model/a");
        assert_eq!(stored.field, "entity.body");
        assert_eq!(stored.text_fingerprint, Some(expected.clone()));
        let raw: String = pool
            .try_writer()
            .unwrap()
            .conn()
            .query_row(
                "SELECT text_fingerprint FROM vector_provenance \
                 WHERE model_key = 'provenance_test' AND subject_id = ?1",
                [subject_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(raw, expected.as_str());
        let (stored_digest, live_embedding): (String, Vec<u8>) = pool
            .try_writer()
            .unwrap()
            .conn()
            .query_row(
                "SELECT p.embedding_digest, v.embedding \
                 FROM vector_provenance AS p JOIN vec_provenance_test AS v \
                   ON v.subject_id = p.subject_id \
                 WHERE p.model_key = 'provenance_test' AND p.subject_id = ?1",
                [subject_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            stored_digest,
            blake3::hash(&live_embedding).to_hex().to_string()
        );

        store
            .insert_batch(vec![record(subject_id, original_text, &timestamp)])
            .await
            .expect("re-embed unchanged text");
        assert_eq!(
            store
                .provenance(subject_id)
                .await
                .unwrap()
                .unwrap()
                .text_fingerprint,
            Some(expected.clone())
        );

        store
            .insert_batch(vec![record(
                subject_id,
                "rendered title\nchanged body",
                &timestamp,
            )])
            .await
            .expect("re-embed changed text");
        let changed = store.provenance(subject_id).await.unwrap().unwrap();
        assert_eq!(
            changed.text_fingerprint,
            Some(ContentRef::from_digest_bytes(
                blake3::hash(b"rendered title\nchanged body").as_bytes()
            ))
        );
        assert_ne!(changed.text_fingerprint, Some(expected));
    }

    #[tokio::test]
    async fn bypass_vector_replacement_makes_stale_sidecar_unknown() {
        let (pool, store) = make_store();
        let subject_id = Uuid::new_v4();
        let timestamp = DateTime::parse_from_rfc3339("2026-09-25T12:34:56Z")
            .unwrap()
            .with_timezone(&Utc);
        store
            .insert_batch(vec![record(subject_id, "original text", &timestamp)])
            .await
            .expect("index original text");

        {
            let writer = pool.try_writer().unwrap();
            writer
                .conn()
                .execute(
                    "DELETE FROM vec_provenance_test WHERE subject_id = ?1",
                    [subject_id.to_string()],
                )
                .unwrap();
            writer
                .conn()
                .execute(
                    "INSERT INTO vec_provenance_test \
                     (subject_id, namespace, kind, field, embedding_model, embedding) \
                     VALUES (?1, 'ns:test', 'entity', 'entity.body', 'model/a', ?2)",
                    rusqlite::params![subject_id.to_string(), f32_slice_as_bytes(&[0.7_f32, 0.8])],
                )
                .unwrap();
        }
        let present = store.provenance(subject_id).await.unwrap().unwrap();
        assert_eq!(present.embedding_model, "model/a");
        assert_eq!(present.field, "entity.body");
        assert_eq!(present.text_fingerprint, None);
        assert_eq!(present.updated_at, None);
    }

    #[tokio::test]
    async fn model_scoped_identical_blob_provenance() {
        let (pool, first_store) = make_store();
        pool.try_writer()
            .unwrap()
            .conn()
            .execute_batch(
                "CREATE VIRTUAL TABLE vec_provenance_other USING vec0(\
                 subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, \
                 kind TEXT NOT NULL, field TEXT NOT NULL, \
                 embedding_model TEXT NOT NULL, embedding float[2] distance_metric=cosine)",
            )
            .unwrap();
        let second_store = SqliteVecStore::new(
            Arc::clone(&pool),
            false,
            "provenance_other".into(),
            "model/b".into(),
            2,
            "ns:test".into(),
        )
        .unwrap();
        let subject_id = Uuid::new_v4();
        let first_timestamp = DateTime::parse_from_rfc3339("2026-09-25T12:34:56.123456789Z")
            .unwrap()
            .with_timezone(&Utc);
        let second_timestamp = DateTime::parse_from_rfc3339("2026-09-25T12:34:57.987654321Z")
            .unwrap()
            .with_timezone(&Utc);
        first_store
            .insert_batch(vec![record(subject_id, "model a source", &first_timestamp)])
            .await
            .unwrap();
        let mut other_record = record(subject_id, "model b source", &second_timestamp);
        other_record.embedding_model = Some("model/b".into());
        second_store.insert_batch(vec![other_record]).await.unwrap();

        let first = first_store.provenance(subject_id).await.unwrap().unwrap();
        let second = second_store.provenance(subject_id).await.unwrap().unwrap();
        assert_eq!(
            first.text_fingerprint,
            Some(VectorRecord::fingerprint_text("model a source"))
        );
        assert_eq!(first.updated_at, Some(first_timestamp));
        assert_eq!(
            second.text_fingerprint,
            Some(VectorRecord::fingerprint_text("model b source"))
        );
        assert_eq!(second.updated_at, Some(second_timestamp));

        let writer = pool.try_writer().unwrap();
        let first_blob: Vec<u8> = writer
            .conn()
            .query_row(
                "SELECT embedding FROM vec_provenance_test WHERE subject_id = ?1",
                [subject_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let second_blob: Vec<u8> = writer
            .conn()
            .query_row(
                "SELECT embedding FROM vec_provenance_other WHERE subject_id = ?1",
                [subject_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            first_blob, second_blob,
            "the digests alone cannot separate models"
        );
        assert_eq!(
            joined_provenance_count(
                writer.conn(),
                "vec_provenance_test",
                "provenance_test",
                subject_id,
                "ns:test"
            ),
            1
        );
        assert_eq!(
            joined_provenance_count(
                writer.conn(),
                "vec_provenance_other",
                "provenance_other",
                subject_id,
                "ns:test"
            ),
            1
        );
    }

    #[tokio::test]
    async fn namespace_scoped_identical_blob_provenance() {
        let (pool, source_store) = make_store();
        let subject_id = Uuid::new_v4();
        let timestamp = DateTime::parse_from_rfc3339("2026-09-25T12:34:56.123456789Z")
            .unwrap()
            .with_timezone(&Utc);
        source_store
            .insert_batch(vec![record(subject_id, "source namespace", &timestamp)])
            .await
            .unwrap();

        {
            let writer = pool.try_writer().unwrap();
            let live_blob: Vec<u8> = writer
                .conn()
                .query_row(
                    "SELECT embedding FROM vec_provenance_test WHERE subject_id = ?1",
                    [subject_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            writer.conn().execute_batch("BEGIN IMMEDIATE").unwrap();
            writer
                .conn()
                .execute(
                    "DELETE FROM vec_provenance_test WHERE subject_id = ?1",
                    [subject_id.to_string()],
                )
                .unwrap();
            writer
                .conn()
                .execute(
                    "INSERT INTO vec_provenance_test \
                     (subject_id, namespace, kind, field, embedding_model, embedding) \
                     VALUES (?1, 'ns:other', 'entity', 'entity.body', 'model/a', ?2)",
                    rusqlite::params![subject_id.to_string(), live_blob],
                )
                .unwrap();
            writer.conn().execute_batch("COMMIT").unwrap();
            let sidecar_namespace: String = writer
                .conn()
                .query_row(
                    "SELECT namespace FROM vector_provenance \
                     WHERE model_key = 'provenance_test' AND subject_id = ?1",
                    [subject_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(sidecar_namespace, "ns:test");
            assert_eq!(
                joined_provenance_count(
                    writer.conn(),
                    "vec_provenance_test",
                    "provenance_test",
                    subject_id,
                    "ns:other"
                ),
                0
            );
        }

        let destination_store = SqliteVecStore::new(
            Arc::clone(&pool),
            false,
            "provenance_test".into(),
            "model/a".into(),
            2,
            "ns:other".into(),
        )
        .unwrap();
        assert!(source_store.provenance(subject_id).await.unwrap().is_none());
        let moved = destination_store
            .provenance(subject_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(moved.embedding_model, "model/a");
        assert_eq!(moved.field, "entity.body");
        assert_eq!(moved.text_fingerprint, None);
        assert_eq!(moved.updated_at, None);
    }

    #[tokio::test]
    async fn vector_provenance_timestamp_round_trips_and_unattributed_update_clears_it() {
        let (_, store) = make_store();
        let subject_id = Uuid::new_v4();
        let timestamp = DateTime::parse_from_rfc3339("2026-09-25T12:34:56.123456789Z")
            .unwrap()
            .with_timezone(&Utc);
        store
            .insert_batch(vec![record(subject_id, "indexed body", &timestamp)])
            .await
            .expect("index record");
        let persisted = store.provenance(subject_id).await.unwrap().unwrap();
        assert_eq!(persisted.updated_at, Some(timestamp));

        store
            .update(
                subject_id,
                SubstrateKind::Entity,
                "ns:test",
                "entity.body",
                vec![vec![0.3, 0.4]],
            )
            .await
            .expect("replace without source text");
        let replaced = store.provenance(subject_id).await.unwrap().unwrap();
        assert_eq!(replaced.text_fingerprint, None);
        assert_eq!(replaced.updated_at, None);
    }

    #[tokio::test]
    async fn low_level_same_blob_reinsert_clears_provenance() {
        let (pool, store) = make_store();
        let timestamp = DateTime::parse_from_rfc3339("2026-09-25T12:34:56.123456789Z")
            .unwrap()
            .with_timezone(&Utc);
        for operation in ["insert", "update", "insert_exact_only"] {
            let subject_id = Uuid::new_v4();
            store
                .insert_batch(vec![record(subject_id, "attributed source", &timestamp)])
                .await
                .unwrap();
            let before: Vec<u8> = pool
                .try_writer()
                .unwrap()
                .conn()
                .query_row(
                    "SELECT embedding FROM vec_provenance_test WHERE subject_id = ?1",
                    [subject_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            match operation {
                "insert" => {
                    store
                        .insert(
                            subject_id,
                            SubstrateKind::Entity,
                            "ns:test",
                            "entity.body",
                            vec![vec![0.1, 0.2]],
                        )
                        .await
                        .unwrap();
                }
                "update" => {
                    store
                        .update(
                            subject_id,
                            SubstrateKind::Entity,
                            "ns:test",
                            "entity.body",
                            vec![vec![0.1, 0.2]],
                        )
                        .await
                        .unwrap();
                }
                "insert_exact_only" => {
                    store
                        .insert_exact_only(
                            subject_id,
                            SubstrateKind::Entity,
                            "ns:test",
                            "entity.body",
                            vec![vec![0.1, 0.2]],
                        )
                        .await
                        .unwrap();
                }
                _ => unreachable!(),
            }

            let writer = pool.try_writer().unwrap();
            let after: Vec<u8> = writer
                .conn()
                .query_row(
                    "SELECT embedding FROM vec_provenance_test WHERE subject_id = ?1",
                    [subject_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(after, before, "{operation} must replace with the same BLOB");
            let fields: (Option<String>, Option<String>) = writer
                .conn()
                .query_row(
                    "SELECT text_fingerprint, updated_at FROM vector_provenance \
                     WHERE model_key = 'provenance_test' AND subject_id = ?1",
                    [subject_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(
                fields,
                (None, None),
                "{operation} must clear sidecar fields"
            );
            drop(writer);
            let observed = store.provenance(subject_id).await.unwrap().unwrap();
            assert_eq!(observed.text_fingerprint, None, "{operation}");
            assert_eq!(observed.updated_at, None, "{operation}");
        }
    }

    #[tokio::test]
    async fn failed_replacement_rolls_back_vector_and_bound_provenance() {
        let (_, store) = make_store();
        let subject_id = Uuid::new_v4();
        let timestamp = DateTime::parse_from_rfc3339("2026-09-25T12:34:56Z")
            .unwrap()
            .with_timezone(&Utc);
        store
            .insert_batch(vec![record(subject_id, "original text", &timestamp)])
            .await
            .unwrap();
        let original = store.provenance(subject_id).await.unwrap().unwrap();
        let mut replacement = record(subject_id, "replacement text", &timestamp);
        replacement.vectors = vec![vec![0.7, 0.8]];

        let _guard = failpoint::FailpointGuard::new();
        let summary = store.insert_batch(vec![replacement]).await.unwrap();
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.affected, 0);
        assert_eq!(
            store.provenance(subject_id).await.unwrap().unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn legacy_and_deleted_vectors_have_no_current_provenance() {
        let (pool, store) = make_store();
        let subject_id = Uuid::new_v4();
        {
            let writer = pool.try_writer().unwrap();
            writer
                .conn()
                .execute_batch("DROP TABLE vector_provenance")
                .expect("simulate pre-migration database");
            writer
                .conn()
                .execute(
                    "INSERT INTO vec_provenance_test \
                     (subject_id, namespace, kind, field, embedding_model, embedding) \
                     VALUES (?1, 'ns:test', 'entity', 'entity.body', 'model/a', ?2)",
                    rusqlite::params![subject_id.to_string(), f32_slice_as_bytes(&[0.1_f32, 0.2])],
                )
                .expect("write legacy vector without provenance");
            writer
                .conn()
                .execute_batch(crate::migrations::VECTOR_PROVENANCE_DDL)
                .expect("migrate provenance sidecar");
        }
        let legacy = store.provenance(subject_id).await.unwrap().unwrap();
        assert_eq!(legacy.text_fingerprint, None);
        assert_eq!(legacy.updated_at, None);

        let timestamp = DateTime::parse_from_rfc3339("2026-09-25T12:34:56Z")
            .unwrap()
            .with_timezone(&Utc);
        store
            .insert_batch(vec![record(subject_id, "new text", &timestamp)])
            .await
            .expect("replace legacy vector");
        let other_namespace = SqliteVecStore::new(
            Arc::clone(&pool),
            false,
            "provenance_test".into(),
            "model/a".into(),
            2,
            "ns:other".into(),
        )
        .unwrap();
        assert!(!other_namespace.delete(subject_id).await.unwrap());
        assert!(store
            .provenance(subject_id)
            .await
            .unwrap()
            .unwrap()
            .text_fingerprint
            .is_some());
        assert!(store.delete(subject_id).await.expect("delete vector"));
        assert!(store.provenance(subject_id).await.unwrap().is_none());
        let sidecar_count: i64 = pool
            .try_writer()
            .unwrap()
            .conn()
            .query_row(
                "SELECT count(*) FROM vector_provenance \
                 WHERE model_key = 'provenance_test' AND subject_id = ?1",
                [subject_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sidecar_count, 0);
    }
}

#[cfg(test)]
#[path = "vectors_busy_tests.rs"]
mod direct_busy_tests;
