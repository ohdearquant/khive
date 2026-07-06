//! SQLite-backed `SparseStore` implementation.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use khive_score::DeterministicScore;
use khive_storage::error::StorageError;
use khive_storage::types::{
    BatchWriteSummary, SparseRecord, SparseSearchHit, SparseSearchRequest, SparseVector,
};
use khive_storage::{SparseStore, StorageCapability};
use khive_types::SubstrateKind;

use crate::error::SqliteError;
use crate::pool::ConnectionPool;
use crate::writer_task::WriterTaskHandle;

fn map_err(e: rusqlite::Error, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Sparse, op, e)
}

fn map_sqlite_err(e: SqliteError, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Sparse, op, e)
}

/// Validate that a sparse vector is well-formed.
///
/// - indices and values must have equal lengths
/// - at least one element
/// - all values must be finite
/// - indices must be strictly increasing (no duplicates)
fn validate_sparse_vector(vector: &SparseVector, op: &'static str) -> Result<(), StorageError> {
    if vector.indices.len() != vector.values.len() {
        return Err(StorageError::InvalidInput {
            capability: StorageCapability::Sparse,
            operation: op.into(),
            message: format!(
                "indices length ({}) != values length ({})",
                vector.indices.len(),
                vector.values.len()
            ),
        });
    }
    if vector.indices.is_empty() {
        return Err(StorageError::InvalidInput {
            capability: StorageCapability::Sparse,
            operation: op.into(),
            message: "sparse vector must have at least one element".into(),
        });
    }
    for (i, v) in vector.values.iter().enumerate() {
        if !v.is_finite() {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Sparse,
                operation: op.into(),
                message: format!("non-finite value at position {i}: {v}"),
            });
        }
    }
    // Verify strictly increasing indices.
    for window in vector.indices.windows(2) {
        if window[0] >= window[1] {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Sparse,
                operation: op.into(),
                message: format!(
                    "indices must be strictly increasing; found {} then {}",
                    window[0], window[1]
                ),
            });
        }
    }
    Ok(())
}

/// Serialize f32 slice to little-endian bytes (same pattern as vectors.rs).
fn f32_slice_as_bytes(data: &[f32]) -> &[u8] {
    // SAFETY: same safety argument as vectors.rs — valid &[f32], alignment = 1, lifetime tied to input.
    unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, std::mem::size_of_val(data)) }
}

/// DML-only batch insert loop shared by both the legacy (flag-off) and
/// WriterTask-routed (flag-on) `insert_sparse_batch` paths (ADR-067
/// Component A).
///
/// Issues no `BEGIN` / `COMMIT` / `ROLLBACK` itself — the caller owns the
/// enclosing transaction. Per-row failures (validation or SQL) are captured
/// into `BatchWriteSummary::failed`/`first_error` rather than aborting the
/// loop, matching the existing partial-success contract.
fn batch_insert_sparse_dml(
    conn: &rusqlite::Connection,
    table: &str,
    records: &[SparseRecord],
    attempted: u64,
) -> Result<BatchWriteSummary, rusqlite::Error> {
    let sql = format!(
        "INSERT INTO {table} \
         (subject_id, namespace, kind, field, indices_json, values_blob, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT(subject_id, namespace, field) DO UPDATE SET \
         indices_json = excluded.indices_json, \
         values_blob = excluded.values_blob, \
         updated_at = excluded.updated_at"
    );

    let mut affected = 0u64;
    let mut failed = 0u64;
    let mut first_error = String::new();

    for record in records {
        // Validate inline — skip invalid records rather than aborting the batch.
        if record.vector.indices.len() != record.vector.values.len()
            || record.vector.indices.is_empty()
            || record.vector.values.iter().any(|v| !v.is_finite())
            || record.vector.indices.windows(2).any(|w| w[0] >= w[1])
        {
            if first_error.is_empty() {
                first_error = format!("invalid sparse vector for subject {}", record.subject_id);
            }
            failed += 1;
            continue;
        }

        let indices_json = match serde_json::to_string(&record.vector.indices) {
            Ok(j) => j,
            Err(e) => {
                if first_error.is_empty() {
                    first_error = e.to_string();
                }
                failed += 1;
                continue;
            }
        };
        let values_blob = f32_slice_as_bytes(&record.vector.values);
        let now = record.updated_at.timestamp();
        let id_str = record.subject_id.to_string();
        let kind_str = record.kind.to_string();

        match conn.execute(
            &sql,
            rusqlite::params![
                &id_str,
                &record.namespace,
                &kind_str,
                &record.field,
                &indices_json,
                values_blob,
                now
            ],
        ) {
            Ok(_) => affected += 1,
            Err(e) => {
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

/// Create the sparse table and its index for the given model_key.
pub(crate) fn ensure_sparse_schema(
    conn: &rusqlite::Connection,
    model_key: &str,
) -> Result<(), rusqlite::Error> {
    let table = format!("sparse_{}", model_key);
    let ddl = format!(
        "CREATE TABLE IF NOT EXISTS {table} (\
         subject_id TEXT NOT NULL, \
         namespace TEXT NOT NULL, \
         kind TEXT NOT NULL, \
         field TEXT NOT NULL, \
         indices_json TEXT NOT NULL, \
         values_blob BLOB NOT NULL, \
         updated_at INTEGER NOT NULL, \
         PRIMARY KEY(subject_id, namespace, field)\
         ); \
         CREATE INDEX IF NOT EXISTS idx_{table}_namespace_kind \
         ON {table}(namespace, kind);"
    );
    conn.execute_batch(&ddl)
}

/// SQLite-backed sparse vector store.
pub struct SqliteSparseStore {
    pool: Arc<ConnectionPool>,
    is_file_backed: bool,
    table_name: String,
    namespace: String,
    writer_task: Option<WriterTaskHandle>,
}

impl SqliteSparseStore {
    /// Create a new sparse store for the given model key and namespace.
    pub fn new(
        pool: Arc<ConnectionPool>,
        is_file_backed: bool,
        model_key: String,
        namespace: String,
    ) -> Result<Self, SqliteError> {
        let table_name = format!("sparse_{}", model_key);
        // Best-effort opt-in (ADR-067 Component A, mirrors entity.rs slice 1
        // policy): a missing writer task degrades to the legacy pool-mutex
        // path rather than failing construction.
        let writer_task = pool.writer_task_handle().ok().flatten();
        Ok(Self {
            pool,
            is_file_backed,
            table_name,
            namespace,
            writer_task,
        })
    }

    /// Route a single-row write through the pool-wide `WriterTask` when
    /// `KHIVE_WRITE_QUEUE=1` and a handle is available; otherwise fall back
    /// to the legacy pool-mutex path (ADR-067 Component A, Fork C slice 2).
    ///
    /// This is the ONE routing point for every `with_writer` caller in this
    /// store (`upsert_sparse_vector`, `delete_sparse_subject`). `f` must be
    /// DML-only — on the flag-on path it runs inside the WriterTask's own
    /// transaction, so a bare `BEGIN IMMEDIATE` would violate SQLite's
    /// nested-transaction rule. `insert_sparse_batch` (the batch method)
    /// does its own flag check and returns early on `Some`, so its
    /// fallback call into this helper only ever executes on the flag-off
    /// path (`self.writer_task` is `None` by construction whenever that
    /// call is reached) — no double-routing.
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

        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let guard = pool.try_writer().map_err(|e| map_sqlite_err(e, op))?;
            f(guard.conn()).map_err(|e| map_err(e, op))
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Sparse, op, e))?
    }

    async fn with_reader<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        if self.is_file_backed {
            // For file-backed DBs open a standalone read-only connection.
            let config = self.pool.config();
            let path = config.path.as_ref().ok_or_else(|| StorageError::Pool {
                operation: "sparse_reader".into(),
                message: "in-memory databases do not support standalone connections".into(),
            })?;
            let conn = rusqlite::Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | rusqlite::OpenFlags::SQLITE_OPEN_URI,
            )
            .map_err(|e| map_err(e, op))?;
            tokio::task::spawn_blocking(move || f(&conn).map_err(|e| map_err(e, op)))
                .await
                .map_err(|e| StorageError::driver(StorageCapability::Sparse, op, e))?
        } else {
            let pool = Arc::clone(&self.pool);
            tokio::task::spawn_blocking(move || {
                let guard = pool.reader().map_err(|e| map_sqlite_err(e, op))?;
                f(guard.conn()).map_err(|e| map_err(e, op))
            })
            .await
            .map_err(|e| StorageError::driver(StorageCapability::Sparse, op, e))?
        }
    }

    async fn upsert_sparse_vector(
        &self,
        subject_id: Uuid,
        kind: SubstrateKind,
        namespace: &str,
        field: &str,
        vector: SparseVector,
    ) -> Result<(), StorageError> {
        let table = self.table_name.clone();
        let ns = namespace.to_string();
        let field = field.to_string();
        let id_str = subject_id.to_string();
        let kind_str = kind.to_string();

        self.with_writer("sparse_upsert", move |conn| {
            let indices_json = serde_json::to_string(&vector.indices).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            let values_blob = f32_slice_as_bytes(&vector.values);
            let now = chrono::Utc::now().timestamp();
            let sql = format!(
                "INSERT INTO {table} \
                 (subject_id, namespace, kind, field, indices_json, values_blob, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT(subject_id, namespace, field) DO UPDATE SET \
                 kind = excluded.kind, \
                 indices_json = excluded.indices_json, \
                 values_blob = excluded.values_blob, \
                 updated_at = excluded.updated_at"
            );
            conn.execute(
                &sql,
                rusqlite::params![
                    &id_str,
                    &ns,
                    &kind_str,
                    &field,
                    &indices_json,
                    values_blob,
                    now
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn insert_sparse_batch(
        &self,
        records: Vec<SparseRecord>,
    ) -> Result<BatchWriteSummary, StorageError> {
        let table = self.table_name.clone();
        let attempted = records.len() as u64;

        // ADR-067 Component A: when the write queue is enabled, route
        // through the pool-wide WriterTask. DML-only closure — no BEGIN
        // IMMEDIATE/COMMIT/ROLLBACK here, since the WriterTask's run loop
        // owns the transaction.
        if let Some(writer_task) = &self.writer_task {
            let table2 = table.clone();
            return writer_task
                .send(move |conn| {
                    batch_insert_sparse_dml(conn, &table2, &records, attempted)
                        .map_err(|e| map_err(e, "sparse_insert_batch"))
                })
                .await;
        }

        // Flag-off (default) path: byte-for-byte unchanged from pre-ADR-067
        // behavior — the closure owns its own BEGIN IMMEDIATE/COMMIT.
        self.with_writer("sparse_insert_batch", move |conn| {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let _tx_handle =
                khive_storage::tx_registry::register(Some("sparse_insert_batch".to_string()));

            let summary = batch_insert_sparse_dml(conn, &table, &records, attempted)?;

            conn.execute_batch("COMMIT")?;
            Ok(summary)
        })
        .await
    }

    async fn delete_sparse_subject(&self, subject_id: Uuid) -> Result<bool, StorageError> {
        let table = self.table_name.clone();
        let namespace = self.namespace.clone();
        let id_str = subject_id.to_string();

        self.with_writer("sparse_delete", move |conn| {
            let sql = format!("DELETE FROM {table} WHERE subject_id = ?1 AND namespace = ?2");
            let deleted = conn.execute(&sql, rusqlite::params![&id_str, &namespace])?;
            Ok(deleted > 0)
        })
        .await
    }

    async fn search_sparse_vectors(
        &self,
        request: SparseSearchRequest,
    ) -> Result<Vec<SparseSearchHit>, StorageError> {
        request
            .validate()
            .map_err(|message| StorageError::InvalidInput {
                capability: StorageCapability::Sparse,
                operation: "sparse_search".into(),
                message,
            })?;

        let table = self.table_name.clone();
        let ns = request
            .namespace
            .clone()
            .unwrap_or_else(|| self.namespace.clone());
        let kind_filter = request.kind.map(|k| k.to_string());
        let query = request.query;
        let top_k = usize::try_from(request.top_k).map_err(|_| StorageError::InvalidInput {
            capability: StorageCapability::Sparse,
            operation: "sparse_search".into(),
            message: "SparseSearchRequest: top_k does not fit usize".into(),
        })?;
        let heap_capacity = top_k
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidInput {
                capability: StorageCapability::Sparse,
                operation: "sparse_search".into(),
                message: "SparseSearchRequest: top_k capacity overflow".into(),
            })?;

        self.with_reader("sparse_search", move |conn| {
            // Load candidate rows for namespace (and optional kind).
            let (sql, kind_str_ref) = if let Some(ref kind_str) = kind_filter {
                (
                    format!(
                        "SELECT subject_id, indices_json, values_blob \
                         FROM {table} WHERE namespace = ?1 AND kind = ?2"
                    ),
                    Some(kind_str.as_str()),
                )
            } else {
                (
                    format!(
                        "SELECT subject_id, indices_json, values_blob \
                         FROM {table} WHERE namespace = ?1"
                    ),
                    None,
                )
            };

            let mut stmt = conn.prepare(&sql)?;

            // Collect rows.
            let rows: Vec<rusqlite::Result<(String, String, Vec<u8>)>> =
                if let Some(kind_str) = kind_str_ref {
                    stmt.query_map(rusqlite::params![&ns, kind_str], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })?
                    .collect()
                } else {
                    stmt.query_map(rusqlite::params![&ns], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })?
                    .collect()
                };

            // Bounded min-heap for top-k selection (KDB-AUD-003).
            let mut heap: BinaryHeap<Reverse<ScoredCandidate>> =
                BinaryHeap::with_capacity(heap_capacity);

            for row_result in rows {
                let (id_str, indices_json, values_blob) = row_result?;

                let subject_id = Uuid::parse_str(&id_str).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;

                // surface malformed rows as errors instead of silently skipping them
                let stored_indices: Vec<u32> =
                    serde_json::from_str(&indices_json).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::<dyn std::error::Error + Send + Sync>::from(format!(
                                "corrupt sparse row {id_str}: invalid indices JSON: {e}"
                            )),
                        )
                    })?;

                if values_blob.len() % 4 != 0 {
                    return Err(rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Blob,
                        Box::<dyn std::error::Error + Send + Sync>::from(format!(
                            "corrupt sparse row {id_str}: values blob length {} not a multiple of 4",
                            values_blob.len()
                        )),
                    ));
                }

                let stored_values: Vec<f32> = values_blob
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect();

                validate_persisted_sparse(&id_str, &stored_indices, &stored_values)?;

                let score = sparse_dot_product(
                    &query.indices,
                    &query.values,
                    &stored_indices,
                    &stored_values,
                );

                heap.push(Reverse(ScoredCandidate { score, subject_id }));
                if heap.len() > top_k {
                    heap.pop();
                }
            }

            // Drain heap and sort descending by score, ascending by UUID on tie.
            let mut top: Vec<_> = heap.into_iter().map(|Reverse(c)| c).collect();
            top.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.subject_id.cmp(&b.subject_id))
            });

            let hits = top
                .into_iter()
                .enumerate()
                .map(|(i, c)| SparseSearchHit {
                    subject_id: c.subject_id,
                    score: DeterministicScore::from_f64(c.score),
                    rank: (i + 1) as u32,
                })
                .collect();

            Ok(hits)
        })
        .await
    }

    async fn count_sparse_rows(&self) -> Result<u64, StorageError> {
        let table = self.table_name.clone();
        let namespace = self.namespace.clone();
        self.with_reader("sparse_count", move |conn| {
            let sql = format!("SELECT COUNT(*) FROM {table} WHERE namespace = ?1");
            let count: i64 =
                conn.query_row(&sql, rusqlite::params![&namespace], |row| row.get(0))?;
            Ok(count as u64)
        })
        .await
    }
}

/// Candidate scored during sparse search, ordered for a min-heap so we can
/// maintain a bounded top-k set: (score desc, subject_id asc) tie-breaking.
#[derive(PartialEq)]
struct ScoredCandidate {
    score: f64,
    subject_id: Uuid,
}

impl Eq for ScoredCandidate {}

impl PartialOrd for ScoredCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredCandidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Min-heap: lower score pops first. On tie, higher UUID pops first
        // (so lower UUID is retained = deterministic ascending tie-break).
        match self
            .score
            .partial_cmp(&other.score)
            .unwrap_or(std::cmp::Ordering::Equal)
        {
            std::cmp::Ordering::Equal => other.subject_id.cmp(&self.subject_id),
            ord => ord,
        }
    }
}

/// Validate invariants on a deserialized sparse vector from the database.
/// Returns a storage error describing the corruption instead of silently
/// skipping the row (KDB-AUD-002).
fn validate_persisted_sparse(
    subject_id: &str,
    indices: &[u32],
    values: &[f32],
) -> Result<(), rusqlite::Error> {
    if indices.len() != values.len() {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Blob,
            Box::<dyn std::error::Error + Send + Sync>::from(format!(
                "corrupt sparse row {subject_id}: indices len {} != values len {}",
                indices.len(),
                values.len()
            )),
        ));
    }
    for (i, v) in values.iter().enumerate() {
        if !v.is_finite() {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Blob,
                Box::<dyn std::error::Error + Send + Sync>::from(format!(
                    "corrupt sparse row {subject_id}: non-finite value at position {i}: {v}"
                )),
            ));
        }
    }
    for window in indices.windows(2) {
        if window[0] >= window[1] {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Blob,
                Box::<dyn std::error::Error + Send + Sync>::from(format!(
                    "corrupt sparse row {subject_id}: indices not strictly increasing at {} >= {}",
                    window[0], window[1]
                )),
            ));
        }
    }
    Ok(())
}

/// Sparse dot product via merge of two sorted index arrays.
fn sparse_dot_product(q_idx: &[u32], q_val: &[f32], s_idx: &[u32], s_val: &[f32]) -> f64 {
    let mut dot = 0.0f64;
    let mut qi = 0;
    let mut si = 0;
    while qi < q_idx.len() && si < s_idx.len() {
        match q_idx[qi].cmp(&s_idx[si]) {
            std::cmp::Ordering::Equal => {
                dot += q_val[qi] as f64 * s_val[si] as f64;
                qi += 1;
                si += 1;
            }
            std::cmp::Ordering::Less => qi += 1,
            std::cmp::Ordering::Greater => si += 1,
        }
    }
    dot
}

#[async_trait]
impl SparseStore for SqliteSparseStore {
    async fn insert_sparse(
        &self,
        subject_id: Uuid,
        kind: SubstrateKind,
        namespace: &str,
        field: &str,
        vector: SparseVector,
    ) -> Result<(), StorageError> {
        validate_sparse_vector(&vector, "sparse_insert")?;
        self.upsert_sparse_vector(subject_id, kind, namespace, field, vector)
            .await
    }

    async fn insert_batch(
        &self,
        records: Vec<SparseRecord>,
    ) -> Result<BatchWriteSummary, StorageError> {
        self.insert_sparse_batch(records).await
    }

    async fn delete(&self, subject_id: Uuid) -> Result<bool, StorageError> {
        self.delete_sparse_subject(subject_id).await
    }

    async fn search_sparse(
        &self,
        request: SparseSearchRequest,
    ) -> Result<Vec<SparseSearchHit>, StorageError> {
        validate_sparse_vector(&request.query, "sparse_search")?;
        self.search_sparse_vectors(request).await
    }

    async fn count(&self) -> Result<u64, StorageError> {
        self.count_sparse_rows().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::{ConnectionPool, PoolConfig};

    fn make_store(model_key: &str) -> SqliteSparseStore {
        let config = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(config).expect("pool"));
        // Create schema.
        {
            let writer = pool.try_writer().expect("writer");
            ensure_sparse_schema(writer.conn(), model_key).expect("schema");
        }
        SqliteSparseStore::new(pool, false, model_key.to_string(), "ns:test".to_string())
            .expect("store")
    }

    fn sv(indices: Vec<u32>, values: Vec<f32>) -> SparseVector {
        SparseVector { indices, values }
    }

    #[tokio::test]
    async fn insert_and_count() {
        let store = make_store("test_count");
        let id = Uuid::new_v4();
        store
            .insert_sparse(
                id,
                SubstrateKind::Entity,
                "ns:test",
                "body",
                sv(vec![0, 2], vec![1.0, 0.5]),
            )
            .await
            .unwrap();
        assert_eq!(store.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn insert_and_search() {
        let store = make_store("test_search");
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        store
            .insert_sparse(
                id1,
                SubstrateKind::Entity,
                "ns:test",
                "body",
                sv(vec![0, 1], vec![1.0, 0.0]),
            )
            .await
            .unwrap();
        store
            .insert_sparse(
                id2,
                SubstrateKind::Entity,
                "ns:test",
                "body",
                sv(vec![0, 1], vec![0.0, 1.0]),
            )
            .await
            .unwrap();

        let hits = store
            .search_sparse(SparseSearchRequest {
                query: sv(vec![0], vec![1.0]),
                top_k: 2,
                namespace: Some("ns:test".into()),
                kind: None,
            })
            .await
            .unwrap();

        assert!(!hits.is_empty());
        assert_eq!(hits[0].subject_id, id1, "id1 should rank first");
        assert_eq!(hits[0].rank, 1);
    }

    /// STORAGE-AUD-002 / #470: top_k = u32::MAX must return InvalidInput
    /// without allocating a multi-hundred-GB heap.
    #[tokio::test]
    async fn sparse_top_k_u32_max_rejected() {
        let store = make_store("test_top_k_max");
        let id = Uuid::new_v4();
        store
            .insert_sparse(
                id,
                SubstrateKind::Entity,
                "ns:test",
                "body",
                sv(vec![0], vec![1.0]),
            )
            .await
            .unwrap();

        let result = store
            .search_sparse(SparseSearchRequest {
                query: sv(vec![0], vec![1.0]),
                top_k: u32::MAX,
                namespace: Some("ns:test".into()),
                kind: None,
            })
            .await;

        assert!(
            matches!(result, Err(StorageError::InvalidInput { .. })),
            "expected InvalidInput, got {result:?}"
        );
    }

    #[tokio::test]
    async fn delete_removes_row() {
        let store = make_store("test_delete");
        let id = Uuid::new_v4();
        store
            .insert_sparse(
                id,
                SubstrateKind::Entity,
                "ns:test",
                "body",
                sv(vec![1], vec![1.0]),
            )
            .await
            .unwrap();
        assert_eq!(store.count().await.unwrap(), 1);

        let deleted = store.delete(id).await.unwrap();
        assert!(deleted);
        assert_eq!(store.count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn mismatched_lengths_rejected() {
        let store = make_store("test_mismatch");
        let result = store
            .insert_sparse(
                Uuid::new_v4(),
                SubstrateKind::Entity,
                "ns:test",
                "body",
                SparseVector {
                    indices: vec![0, 1],
                    values: vec![1.0],
                },
            )
            .await;
        assert!(matches!(result, Err(StorageError::InvalidInput { .. })));
    }

    #[tokio::test]
    async fn non_finite_values_rejected() {
        let store = make_store("test_nonfinite");
        let result = store
            .insert_sparse(
                Uuid::new_v4(),
                SubstrateKind::Entity,
                "ns:test",
                "body",
                sv(vec![0], vec![f32::NAN]),
            )
            .await;
        assert!(matches!(result, Err(StorageError::InvalidInput { .. })));
    }

    #[tokio::test]
    async fn duplicate_indices_rejected() {
        let store = make_store("test_dup_idx");
        let result = store
            .insert_sparse(
                Uuid::new_v4(),
                SubstrateKind::Entity,
                "ns:test",
                "body",
                sv(vec![0, 0], vec![1.0, 2.0]),
            )
            .await;
        assert!(matches!(result, Err(StorageError::InvalidInput { .. })));
    }

    #[tokio::test]
    async fn empty_vector_rejected() {
        let store = make_store("test_empty");
        let result = store
            .insert_sparse(
                Uuid::new_v4(),
                SubstrateKind::Entity,
                "ns:test",
                "body",
                sv(vec![], vec![]),
            )
            .await;
        assert!(matches!(result, Err(StorageError::InvalidInput { .. })));
    }

    #[tokio::test]
    async fn namespace_isolation() {
        let store = make_store("test_ns_iso");
        let id = Uuid::new_v4();
        store
            .insert_sparse(
                id,
                SubstrateKind::Entity,
                "ns:a",
                "body",
                sv(vec![0], vec![1.0]),
            )
            .await
            .unwrap();

        let hits = store
            .search_sparse(SparseSearchRequest {
                query: sv(vec![0], vec![1.0]),
                top_k: 5,
                namespace: Some("ns:b".into()),
                kind: None,
            })
            .await
            .unwrap();
        assert!(hits.is_empty(), "ns:b should not see ns:a data");
    }

    #[tokio::test]
    async fn insert_batch_happy_path() {
        use chrono::Utc;
        use khive_types::SubstrateKind;

        let store = make_store("test_batch");
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let records = vec![
            SparseRecord {
                subject_id: id1,
                kind: SubstrateKind::Entity,
                namespace: "ns:test".into(),
                field: "body".into(),
                vector: sv(vec![0, 3], vec![0.5, 0.8]),
                updated_at: Utc::now(),
            },
            SparseRecord {
                subject_id: id2,
                kind: SubstrateKind::Entity,
                namespace: "ns:test".into(),
                field: "body".into(),
                vector: sv(vec![1], vec![1.0]),
                updated_at: Utc::now(),
            },
        ];
        let summary = store.insert_batch(records).await.unwrap();
        assert_eq!(summary.attempted, 2);
        assert_eq!(summary.affected, 2);
        assert_eq!(summary.failed, 0);
        assert_eq!(store.count().await.unwrap(), 2);
    }

    /// ADR-067 Component A entry 6: with `KHIVE_WRITE_QUEUE=1`, `insert_batch`
    /// (delegating to `insert_sparse_batch`) routes through the WriterTask
    /// channel instead of the pool-mutex path, and both rows are actually
    /// committed and independently searchable back.
    ///
    /// `#[serial]`: mutates the process-global `KHIVE_WRITE_QUEUE` env var,
    /// shared with `pool.rs`'s own env-override tests in this same test binary.
    #[tokio::test]
    #[serial_test::serial]
    async fn insert_batch_routes_through_writer_task_when_flag_enabled() {
        use chrono::Utc;
        use khive_types::SubstrateKind;

        std::env::set_var("KHIVE_WRITE_QUEUE", "1");

        let model_key = "write_queue_flag_test";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("write_queue_sparse.db");
        let pool_cfg = PoolConfig {
            path: Some(path.clone()),
            ..PoolConfig::default()
        };
        let pool = Arc::new(ConnectionPool::new(pool_cfg).expect("pool"));
        {
            let writer = pool.writer().expect("writer");
            ensure_sparse_schema(writer.conn(), model_key).expect("schema");
        }

        let store = SqliteSparseStore::new(
            Arc::clone(&pool),
            true,
            model_key.to_string(),
            "ns:test".to_string(),
        )
        .expect("store");
        std::env::remove_var("KHIVE_WRITE_QUEUE");

        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let records = vec![
            SparseRecord {
                subject_id: id1,
                kind: SubstrateKind::Entity,
                namespace: "ns:test".into(),
                field: "body".into(),
                vector: sv(vec![0, 3], vec![0.5, 0.8]),
                updated_at: Utc::now(),
            },
            SparseRecord {
                subject_id: id2,
                kind: SubstrateKind::Entity,
                namespace: "ns:test".into(),
                field: "body".into(),
                vector: sv(vec![1], vec![1.0]),
                updated_at: Utc::now(),
            },
        ];

        let summary = store.insert_batch(records).await.unwrap();
        assert_eq!(summary.attempted, 2);
        assert_eq!(summary.affected, 2);
        assert_eq!(summary.failed, 0);
        assert_eq!(store.count().await.unwrap(), 2);
        assert_eq!(
            pool.writer_task_spawn_count(),
            1,
            "the flag-ON path must actually spawn and use the writer task"
        );
    }
}
