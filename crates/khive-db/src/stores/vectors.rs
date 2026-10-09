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
use khive_storage::VectorStore;
use khive_storage::{encode_f32_native, StorageResult};
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

mod vector_store_impl;

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
        self.score_candidates_with_kind(query_embedding, candidate_ids, None)
            .await
    }

    async fn score_candidates_with_kind(
        &self,
        query_embedding: &[f32],
        candidate_ids: &[Uuid],
        kind: Option<SubstrateKind>,
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
        let kind_filter = kind.map(|kind| kind.to_string());
        let ids: Vec<String> = candidate_ids.iter().map(|id| id.to_string()).collect();

        self.with_reader("score_candidates", move |conn| {
            let mut all_hits: Vec<VectorSearchHit> = Vec::new();
            let query_blob = encode_f32_native(&query_vec);
            let sql = format!(
                "SELECT e.subject_id, vec_distance_cosine(e.embedding, ?1) as distance \
                 FROM {table} e \
                 WHERE e.namespace = ?2 AND e.embedding_model = ?3 \
                   AND e.subject_id = ?4 AND (?5 IS NULL OR e.kind = ?5)"
            );
            let mut stmt = conn.prepare(&sql)?;

            // Preserve IN's duplicate suppression within each original group,
            // including repeated hits for IDs supplied in different groups.
            for chunk in ids.chunks(399) {
                let mut seen = HashSet::with_capacity(chunk.len());
                for id in chunk.iter().filter(|id| seen.insert(*id)) {
                    let row: Option<(String, f64)> = stmt
                        .query_row(
                            rusqlite::params![
                                query_blob,
                                &namespace,
                                &embedding_model,
                                id,
                                &kind_filter
                            ],
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
/// instead — see the flag-on branches in the `vector_store_impl` module).
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
