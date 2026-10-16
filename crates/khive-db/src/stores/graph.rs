//! SQL-backed `GraphStore`: edge CRUD, neighbor queries, and bounded BFS traversal.

mod edge_reads;
mod edge_statements;
mod edge_upserts;
mod mutation_plans;
mod store_impl;
mod traversal;

use store_impl::*;

use traversal::*;

use edge_upserts::*;

pub(crate) use edge_reads::*;

pub use mutation_plans::*;

pub use edge_statements::*;

#[path = "graph/write_transaction.rs"]
mod write_transaction;
use write_transaction::run_graph_mutation_transaction;

#[path = "graph/guarded_insert.rs"]
mod guarded_insert;
use guarded_insert::{check_statement_guards, insert_edge_if_absent};

#[path = "graph/symmetric_update.rs"]
mod symmetric_update;
pub use symmetric_update::{
    edge_symmetric_conflict_probe_statement, edge_symmetric_delete_noncanonical_statement,
    edge_symmetric_update_inplace_statement, EDGE_SYMMETRIC_CONFLICT_PROBE_SQL,
    EDGE_SYMMETRIC_DELETE_NONCANONICAL_GUARDED_SQL, EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL,
    EDGE_SYMMETRIC_UPDATE_INPLACE_SQL,
};

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::OptionalExtension;
use uuid::Uuid;

use khive_storage::error::StorageError;
use khive_storage::graph::{
    CommitAnnotationGuard, CommitAnnotationInsertOutcome, EdgeInsertIfAbsentOutcome,
    GraphStatementGuard, GraphStatementGuardFailure, SymmetricEdgeUpdateOutcome,
    SymmetricEdgeUpdateRequest,
};
use khive_storage::types::{
    BatchWriteSummary, DeleteMode, DirectedNeighborHit, Direction, Edge, EdgeEndpointBaseCounts,
    EdgeFilter, EdgeSeekPage, EdgeSortField, EdgeUpsertDisposition, EdgeUpsertRefusal,
    EdgeUpsertRequest, EdgeUpsertResult, GraphPath, GuardedBatchOutcome, GuardedBatchRefusal,
    GuardedEdgeBatchRefusal, GuardedEdgeBatchUpsertOutcome, GuardedEdgeUpsertOutcome,
    GuardedWriteOutcome, MissingEndpoints, NeighborCursor, NeighborHit, NeighborQuery, Page,
    PageRequest, PathNode, SeekCursor, SeekPage, SortDirection, SortOrder, SqlStatement, SqlValue,
    TraversalExecutionBudget, TraversalOptions, TraversalRequest,
};
use khive_storage::LinkId;
use khive_storage::StorageCapability;
use khive_storage::{Event, GraphStore, StorageResult};
use khive_types::EdgeRelation;

use crate::error::SqliteError;
use crate::pool::ConnectionPool;
use crate::sql_bridge::bind_params;
use crate::writer_task::WriterTaskHandle;

/// Map a rusqlite error to `StorageError` with `Graph` capability.
fn map_err(e: rusqlite::Error, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Graph, op, e)
}

fn map_sqlite_err(e: SqliteError, op: &'static str) -> StorageError {
    e.into_storage_error(StorageCapability::Graph, op)
}

fn resurrection_required_error(operation: &'static str, edge: &Edge) -> StorageError {
    StorageError::Conflict {
        capability: StorageCapability::Graph,
        operation: operation.into(),
        message: format!(
            "edge {} is soft-deleted; explicit resurrection is required",
            edge.id
        ),
    }
}

const NAMESPACE_COUNT_CHUNK_SIZE: usize = 500;

// Latest eligible annotation of one target. First read at most three incident
// `annotates` edge rows, tombstones included so the probe cannot scan a long
// deleted prefix. The unique (namespace, source, target, relation) index allows
// one row per source, so two or fewer rows is the complete incident set and
// those notes are filtered and ordered directly. Three rows falls back to
// walking notes newest-first, which keeps a long receipt history from being
// enumerated or sorted. That fallback still visits newer unrelated notes when
// a target with three or more incident edges has only old eligible annotations.
const LATEST_ANNOTATING_NOTE_SQL: &str = r#"WITH incident AS MATERIALIZED (
    SELECT source_id, deleted_at
    FROM graph_edges INDEXED BY idx_graph_edges_ns_tgt_rel
    WHERE namespace = ?1 AND target_id = ?2 AND relation = 'annotates'
    LIMIT 3
)
SELECT result.id, result.created_at
FROM notes AS result
WHERE result.id = CASE WHEN (SELECT count(*) FROM incident) <= 2 THEN (
    SELECT n.id
    FROM incident AS e CROSS JOIN notes AS n
    WHERE n.id = e.source_id AND e.deleted_at IS NULL
      AND n.deleted_at IS NULL AND n.kind = ?3
      AND EXISTS (SELECT 1 FROM json_each(CASE
          WHEN json_type(n.properties, '$.tags') = 'array'
          THEN json_extract(n.properties, '$.tags') ELSE '[]' END) AS tag
          WHERE tag.type = 'text' AND tag.value = ?4 COLLATE BINARY)
    ORDER BY n.created_at DESC, n.id ASC LIMIT 1
) ELSE (
    SELECT n.id
    FROM notes AS n INDEXED BY idx_notes_created
    WHERE n.deleted_at IS NULL AND n.kind = ?3
      AND EXISTS (SELECT 1 FROM json_each(CASE
          WHEN json_type(n.properties, '$.tags') = 'array'
          THEN json_extract(n.properties, '$.tags') ELSE '[]' END) AS tag
          WHERE tag.type = 'text' AND tag.value = ?4 COLLATE BINARY)
      AND EXISTS (SELECT 1 FROM graph_edges AS e INDEXED BY idx_graph_edges_unique_triple
          WHERE e.namespace = ?1 AND e.source_id = n.id AND e.target_id = ?2
            AND e.relation = 'annotates' AND e.deleted_at IS NULL)
    ORDER BY n.created_at DESC, n.id ASC LIMIT 1
) END"#;

// The property predicate must run in both branches before ORDER BY/LIMIT;
// filtering the single returned id in a caller lets a newer tagged decoy
// conceal an older receipt with provenance.
const LATEST_ANNOTATING_NOTE_WITH_PROPERTY_SQL: &str = r#"WITH incident AS MATERIALIZED (
    SELECT source_id, deleted_at
    FROM graph_edges INDEXED BY idx_graph_edges_ns_tgt_rel
    WHERE namespace = ?1 AND target_id = ?2 AND relation = 'annotates'
    LIMIT 3
)
SELECT result.id, result.created_at
FROM notes AS result
WHERE result.id = CASE WHEN (SELECT count(*) FROM incident) <= 2 THEN (
    SELECT n.id
    FROM incident AS e CROSS JOIN notes AS n
    WHERE n.id = e.source_id AND e.deleted_at IS NULL
      AND n.deleted_at IS NULL AND n.kind = ?3
      AND EXISTS (SELECT 1 FROM json_each(CASE
          WHEN json_type(n.properties, '$.tags') = 'array'
          THEN json_extract(n.properties, '$.tags') ELSE '[]' END) AS tag
          WHERE tag.type = 'text' AND tag.value = ?4 COLLATE BINARY)
      AND EXISTS (SELECT 1 FROM json_each(n.properties) AS property
          WHERE property.key = ?5 COLLATE BINARY AND property.type = 'text'
            AND property.value = ?6 COLLATE BINARY)
    ORDER BY n.created_at DESC, n.id ASC LIMIT 1
) ELSE (
    SELECT n.id
    FROM notes AS n INDEXED BY idx_notes_created
    WHERE n.deleted_at IS NULL AND n.kind = ?3
      AND EXISTS (SELECT 1 FROM json_each(CASE
          WHEN json_type(n.properties, '$.tags') = 'array'
          THEN json_extract(n.properties, '$.tags') ELSE '[]' END) AS tag
          WHERE tag.type = 'text' AND tag.value = ?4 COLLATE BINARY)
      AND EXISTS (SELECT 1 FROM json_each(n.properties) AS property
          WHERE property.key = ?5 COLLATE BINARY AND property.type = 'text'
            AND property.value = ?6 COLLATE BINARY)
      AND EXISTS (SELECT 1 FROM graph_edges AS e INDEXED BY idx_graph_edges_unique_triple
          WHERE e.namespace = ?1 AND e.source_id = n.id AND e.target_id = ?2
            AND e.relation = 'annotates' AND e.deleted_at IS NULL)
    ORDER BY n.created_at DESC, n.id ASC LIMIT 1
) END"#;

// ---------------------------------------------------------------------------
// Pure statement builders (ADR-099 B3 r6 structural cut) — see entity.rs's
// sibling block for the full rationale. `upsert_edge`/`delete_edge` below and
// `purge_incident_edges` (plus ADR-099's atomic prepare path in
// `khive-runtime`, and `khive-runtime::operations::update_edge`'s
// non-symmetric branch) all call these.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Symmetric-relation update DML — atomic-only, commit-time self-guarding
// variant (ADR-099 §B3).
//
// Canonical `update_edge_symmetric_dml` binds the shared SQL constants in
// `graph/symmetric_update.rs` directly, not the plan-shape builders — the
// builders are used by the atomic prepare path. Canonical probes and branches
// synchronously INSIDE its own writer-task transaction, with no other op
// interleaved between its probe and its write, so the interleaving exposure the
// atomic path has does not arise there. Canonical's absorption delete is
// nonetheless bound to the GUARDED constant, because its snapshot is read
// before the transaction and can be stale by the time the delete runs.
//
// The atomic path is structurally different: its conflict probe runs in the
// async PREPARE phase, which for a multi-op `--atomic` unit completes for
// EVERY op before the synchronous COMMIT phase begins for ANY of them. An
// earlier op in the SAME atomic unit (e.g. a `delete` or another symmetric
// `update` touching the same natural key) can change the conflict landscape
// between this op's prepare-time probe and its own statements finally
// executing at commit time — this is a real staleness
// window, not just an SQL-text duplication concern.
//
// The two builders below close it: instead of a Rust-level `if let
// Some(conflict) { ... } else { ... }` that hand-picks ONE of three
// statements at prepare time (the second hand-assembled branch this
// closes), the atomic plan ALWAYS carries both statements, in order, and
// each is a self-guarding, commit-time predicate that re-evaluates conflict
// state fresh against whatever the transaction's state actually is when it
// runs — not what prepare's probe said:
//
// 1. [`edge_symmetric_delete_if_conflict_statement`]: deletes the requested
//    (non-canonical) row IF AND ONLY IF a differently-id'd canonical row
//    exists at the target natural key at THIS moment AND the row's own
//    `updated_at`/`deleted_at` still match the snapshot this plan was built
//    from (guard: 0 or 1 rows) — a plan built from a since-changed snapshot
//    must not delete the row just because some other, unrelated conflict
//    happens to exist; the second statement's own commit-time predicate
//    (below) then sees `changes() = 0` and fails its `exactly(1)` guard,
//    aborting the whole atomic unit rather than silently absorbing a
//    concurrent writer's change.
// 2. [`edge_symmetric_absorb_or_update_inplace_statement`]: a single
//    `UPDATE` that no longer trusts an `id = ?2 OR natural-key` predicate
//    (ADR-099 §B3 — that predicate could
//    match the WRONG row: if a different op earlier in the SAME atomic unit
//    had already deleted the requested edge, statement 1 above no-ops (its
//    "0 rows" result is indistinguishable at the Rust level from "no
//    conflict existed"), yet the natural-key arm could still hit a
//    pre-existing canonical row that this update never causally touched).
//    The fix ties the natural-key arm to `changes()` — SQLite's per-
//    connection scalar reporting the row count of the most recently
//    COMPLETED statement, i.e. statement 1's own result, evaluated fresh at
//    THIS statement's execution, not at prepare time:
//    - `id = ?2 AND changes() = 0`: statement 1 deleted nothing, so the
//      requested row is still live under its own id — update it in place.
//    - `source_id = ?3 AND target_id = ?4 AND relation = ?5 AND id != ?2
//      AND changes() = 1`: statement 1 just deleted the requested row
//      BECAUSE a conflict existed — match the surviving canonical row and
//      leave its attributes unchanged (ADR-039 DO NOTHING; see below).
//    These two arms are mutually exclusive and, together with statement 1's
//    own guard, jointly exhaustive: if the requested row no longer existed
//    when statement 1 ran (the same-unit race above), statement 1 affects 0
//    rows for a reason unrelated to conflict absorption, `id = ?2` no
//    longer matches anything (the row is gone), and the natural-key arm's
//    `changes() = 1` guard is false — so this statement affects ZERO rows
//    and the plan's `AffectedRowGuard::exactly(1)` on it fails the op,
//    aborting the whole atomic unit rather than silently mutating an
//    unrelated row.
//
//    ADR-039's edge-conflict contract is ON CONFLICT DO NOTHING: the
//    natural-key (absorbed-conflict) arm must leave the surviving canonical
//    row's attributes exactly as they were — refreshing them from the
//    discarded edge (and forcing `deleted_at = NULL`, resurrecting a
//    tombstone) is the same defect already fixed on the merge-rewire path
//    (khive#1213). Every SET expression is therefore keyed on `id = ?2`:
//    that condition is true only for the WHERE clause's first (in-place)
//    arm — the second (absorbed) arm only ever matches a row whose id is
//    NOT ?2 — so each column either takes its new value (in-place arm) or
//    self-assigns its current value (absorbed arm, a true no-op). This
//    still affects exactly one row either way (SQLite's `changes()` counts
//    matched rows, not changed bytes), so `AffectedRowGuard::exactly(1)`
//    and the race-abort behavior above are unaffected by this being a
//    no-op write in the absorbed case.
//
// No probe, no branch, no read at all is needed to APPLY this pair. Which
// row this plan actually touched is derived post-commit by the caller via a
// fresh natural-key lookup (`khive-runtime::KhiveRuntime::get_edge_by_natural_key_including_deleted`,
// filtered on the canonicalized endpoints/relation and including soft-deleted rows — unlike
// `list_edges`, which unconditionally filters `deleted_at IS NULL` and would report "not
// found" for a surviving row this absorption arm left tombstoned) — ADR-099 §B3
// removed the prior prepare-time advisory `target_id` probe entirely:
// a value computed before the
// SAME atomic unit's other ops have run is not a fact this plan can stand
// behind, so result rendering no longer trusts it.

/// A GraphStore backed by SQLite tables.
pub struct SqlGraphStore {
    pool: Arc<ConnectionPool>,
    index_repair: Option<super::index_repair::IndexRepairContext>,
    is_file_backed: bool,
    /// Default namespace for multi-record queries (ADR-007 PARAM-ONLY: used as a
    /// WHERE filter on `query_edges`/`neighbors`/`traverse`, never as an
    /// enforcement gate on by-ID operations).
    namespace: String,
    writer_task: Option<WriterTaskHandle>,
}

impl SqlGraphStore {
    /// Create a new store with a default namespace for multi-record query filtering.
    ///
    /// The namespace is a PARAM-ONLY hint (ADR-007 rule 4) — it is used as a
    /// WHERE filter in multi-record queries and as the write namespace stamped on
    /// upserted edges, but it does NOT enforce isolation: `upsert_edge` accepts
    /// edges from any namespace, and by-ID ops (`get_edge`, `delete_edge`) ignore
    /// the namespace entirely.
    pub fn new_scoped(
        pool: Arc<ConnectionPool>,
        is_file_backed: bool,
        namespace: impl Into<String>,
    ) -> Self {
        // Enabled by default for file-backed pools; explicit off/degraded
        // construction remains synchronous (ADR-067 Component A, mirrors
        // entity.rs policy): a missing writer task is cached without failing
        // construction. Every write re-resolves it and applies
        // strict/compatibility policy then.
        let writer_task = pool.writer_task_handle().ok().flatten();

        Self {
            pool,
            index_repair: None,
            is_file_backed,
            namespace: namespace.into(),
            writer_task,
        }
    }

    pub(crate) fn with_index_repair(
        mut self,
        repair: super::index_repair::IndexRepairContext,
    ) -> Self {
        self.index_repair = Some(repair);
        self
    }

    async fn with_indexed_reader<F, R>(&self, op: &'static str, read: F) -> Result<R, StorageError>
    where
        F: FnMut(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        super::index_repair::run_indexed_read(
            Arc::clone(&self.pool),
            self.index_repair.clone(),
            StorageCapability::Graph,
            op,
            read,
        )
        .await
    }

    fn current_writer_task(
        &self,
        operation: &'static str,
    ) -> Result<Option<WriterTaskHandle>, StorageError> {
        self.pool
            .writer_task_for_write(self.writer_task.as_ref(), operation)
    }

    /// Route a single-row write through the pool-wide `WriterTask` when
    /// the write queue is enabled and a handle is available. Strict mode
    /// refuses a missing handle; compatibility mode falls back to the legacy
    /// standalone-connection / pool-mutex path (ADR-067 Component A, Fork C
    /// slice 2). `f` must be DML-only. See
    /// `crates/khive-db/docs/api/graph.md` for the per-caller routing rules.
    async fn with_writer<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        if let Some(writer_task) = self.current_writer_task(op)? {
            return writer_task
                .send_bounded_at(move |conn| f(conn).map_err(|e| map_err(e, op)), op)
                .await;
        }

        self.pool
            .record_direct_route(crate::timeout_sink::Site::DirectRouteGraphGeneralWrite);
        let pool = Arc::clone(&self.pool);
        let db = crate::timeout_sink::db_label(&pool);
        let is_file_backed = self.is_file_backed;
        tokio::task::spawn_blocking(move || {
            let result =
                pool.execute_direct_transaction(StorageCapability::Graph, op, move |conn| {
                    f(conn).map_err(|error| map_err(error, op))
                });
            if is_file_backed {
                if let Err(error) = &result {
                    crate::timeout_sink::maybe_emit_busy_storage_error(
                        &db,
                        crate::timeout_sink::Site::StandaloneGraph,
                        error,
                    );
                }
            }
            result
        })
        .await
        .map_err(|e| StorageError::driver(StorageCapability::Graph, op, e))?
    }

    async fn observed_edge_write(
        &self,
        operation: &'static str,
        request: EdgeUpsertRequest,
        guard_endpoints: bool,
    ) -> Result<GuardedEdgeUpsertOutcome, StorageError> {
        if let Some(writer_task) = self.current_writer_task(operation)? {
            return writer_task
                .send_bounded_at(
                    move |conn| {
                        observed_edge_upsert(conn, &request, guard_endpoints)
                            .map_err(|error| map_err(error, operation))
                    },
                    operation,
                )
                .await;
        }

        self.with_writer(operation, move |conn| {
            observed_edge_upsert(conn, &request, guard_endpoints)
        })
        .await
    }

    async fn observed_edge_batch_write(
        &self,
        operation: &'static str,
        requests: Vec<EdgeUpsertRequest>,
        guard_endpoints: bool,
    ) -> Result<GuardedEdgeBatchUpsertOutcome, StorageError> {
        if let Some(writer_task) = self.current_writer_task(operation)? {
            return writer_task
                .send_bounded_at(
                    move |conn| {
                        observed_edge_batch_upsert(conn, &requests, guard_endpoints)
                            .map_err(|error| map_err(error, operation))
                    },
                    operation,
                )
                .await;
        }

        self.with_writer(operation, move |conn| {
            observed_edge_batch_upsert(conn, &requests, guard_endpoints)
        })
        .await
    }

    async fn with_reader<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        super::run_pooled_store_read(
            Arc::clone(&self.pool),
            StorageCapability::Graph,
            op,
            move |conn| f(conn).map_err(|error| map_err(error, op)),
        )
        .await
    }
}

// =============================================================================
// Helpers
// =============================================================================

/// Publish a graph read's accounting after the read resolves, on either
/// outcome.
///
/// Both counters are collected inside the storage closure because that closure
/// runs on a blocking thread where the task-local usage context is invisible,
/// and because a value returned only on success reports nothing at all when
/// the read fails partway. `queries` is incremented immediately before a
/// statement executes, so a read that never got that far — a connection the
/// pool could not hand out, a blocking task that died — counts no round trip.
/// `rows` is incremented as each adjacency entry materialises, so entries
/// storage already returned still count when a later row fails to convert.
fn report_graph_usage(queries: &std::sync::atomic::AtomicU64, rows: &std::sync::atomic::AtomicU64) {
    khive_storage::usage::count(
        khive_storage::usage::UsageUnit::DbRoundTrips,
        queries.load(std::sync::atomic::Ordering::Relaxed),
    );
    khive_storage::usage::count(
        khive_storage::usage::UsageUnit::GraphHops,
        rows.load(std::sync::atomic::Ordering::Relaxed),
    );
}

// =============================================================================
// GraphStore implementation
// =============================================================================

impl SqlGraphStore {
    async fn query_neighbors_page(
        &self,
        operation: &'static str,
        node_id: Uuid,
        query: NeighborQuery,
        after: Option<NeighborCursor>,
        neighbor_kinds: Option<Vec<String>>,
    ) -> Result<Vec<NeighborHit>, StorageError> {
        count_neighbor_select();

        let namespace = self.namespace.clone();
        let node_str = node_id.to_string();
        let counted_queries = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counted_rows = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let closure_queries = Arc::clone(&counted_queries);
        let closure_rows = Arc::clone(&counted_rows);
        let result = self
            .with_reader(operation, move |conn| {
                let base_out = "SELECT target_id AS node_id, id AS edge_id, relation, weight \
                            FROM graph_edges \
                            WHERE namespace = ?1 AND source_id = ?2 AND deleted_at IS NULL";
                let base_in = "SELECT source_id AS node_id, id AS edge_id, relation, weight \
                           FROM graph_edges \
                           WHERE namespace = ?1 AND target_id = ?2 AND deleted_at IS NULL";
                let sql = match query.direction {
                    Direction::Out => base_out.to_string(),
                    Direction::In => base_in.to_string(),
                    Direction::Both => format!("{} UNION ALL {}", base_out, base_in),
                };
                let (where_extra, limit_clause, extra_params) =
                    neighbor_extra_clause(&query, 3, after.as_ref(), neighbor_kinds.as_deref());
                let full_sql = format!(
                    "SELECT node_id, edge_id, relation, weight FROM ({}){} \
                 ORDER BY weight DESC, node_id ASC, edge_id ASC{}",
                    sql, where_extra, limit_clause
                );
                let mut stmt = conn.prepare(&full_sql)?;
                let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                all_params.push(Box::new(namespace.clone()));
                all_params.push(Box::new(node_str.clone()));
                all_params.extend(extra_params);
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    all_params.iter().map(|p| p.as_ref()).collect();

                closure_queries.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let rows = stmt.query_map(param_refs.as_slice(), |row| {
                    let nid_str: String = row.get(0)?;
                    let eid_str: String = row.get(1)?;
                    let relation_str: String = row.get(2)?;
                    let weight: f64 = row.get(3)?;
                    Ok((nid_str, eid_str, relation_str, weight))
                })?;
                let mut hits = Vec::new();
                for row in rows {
                    let (nid_str, eid_str, relation_str, weight) = row?;
                    closure_rows.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let relation = relation_str.parse::<EdgeRelation>().map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?;
                    hits.push(NeighborHit {
                        node_id: parse_uuid(&nid_str)?,
                        edge_id: parse_uuid(&eid_str)?,
                        relation,
                        weight,
                        name: None,
                        kind: None,
                        entity_type: None,
                    });
                }
                Ok(hits)
            })
            .await;

        report_graph_usage(&counted_queries, &counted_rows);
        result
    }
}

// =============================================================================
// DDL
// =============================================================================

const GRAPH_DDL: &str = include_str!("../../sql/graph-ddl.sql");

pub(crate) fn ensure_graph_schema(conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(GRAPH_DDL)
}

#[cfg(test)]
#[path = "graph_annotation_tests.rs"]
mod annotation_tests;

#[cfg(test)]
#[path = "graph_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "incident_edge_plan_tests.rs"]
mod incident_edge_plan_tests;

#[cfg(test)]
#[path = "graph_index_repair_tests.rs"]
mod index_repair_tests;

#[cfg(test)]
#[path = "graph_busy_tests.rs"]
mod direct_busy_tests;

#[cfg(test)]
#[path = "graph/statement_guard_tests.rs"]
mod statement_guard_tests;
