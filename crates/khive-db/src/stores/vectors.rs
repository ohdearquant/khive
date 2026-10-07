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

mod dml;
use super::classify_batch_sqlite_error;
pub use dml::delete_subject_from_vector_tables;
use dml::{
    batch_insert_vectors_dml, delete_vector_provenance, delete_vector_subjects_dml,
    log_vector_deletes, orphan_sweep_dml, replace_vector_row_dml, vec_upsert_atomic_dml,
    VectorRowRef,
};

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
mod point_lookup_tests;

#[cfg(test)]
mod unmanaged_write_escalation_tests;

#[cfg(all(test, feature = "vectors"))]
mod batch_exists_tests;

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
mod provenance_tests;

#[cfg(test)]
#[path = "vectors_busy_tests.rs"]
mod direct_busy_tests;
