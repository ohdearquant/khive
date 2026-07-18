//! SQL-backed `GraphStore`: edge CRUD, neighbor queries, and recursive CTE traversal.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use uuid::Uuid;

use khive_storage::error::StorageError;
use khive_storage::types::{
    BatchWriteSummary, DeleteMode, DirectedNeighborHit, Direction, Edge, EdgeFilter, EdgeSeekPage,
    EdgeSortField, GraphPath, GuardedBatchOutcome, GuardedBatchRefusal, GuardedWriteOutcome,
    MissingEndpoints, NeighborHit, NeighborQuery, Page, PageRequest, PathNode, SortDirection,
    SortOrder, SqlStatement, SqlValue, TraversalRequest,
};
use khive_storage::GraphStore;
use khive_storage::LinkId;
use khive_storage::StorageCapability;
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
    StorageError::driver(StorageCapability::Graph, op, e)
}

// ---------------------------------------------------------------------------
// Pure statement builders (ADR-099 B3 r6 structural cut) — see entity.rs's
// sibling block for the full rationale. `upsert_edge`/`delete_edge` below and
// `purge_incident_edges` (plus ADR-099's atomic prepare path in
// `khive-runtime`, and `khive-runtime::operations::update_edge`'s
// non-symmetric branch) all call these.
// ---------------------------------------------------------------------------

/// The natural-key conflict arm's `SET` list — shared, textually, between
/// [`edge_upsert_statement`] and [`edge_insert_guarded_by_endpoints_statement`]
/// (ADR-099 §B3) so the two can never silently
/// diverge again: the atomic link path previously hand-duplicated this SET
/// list without `target_backend = excluded.target_backend`, so a re-link of
/// an edge that had a cross-backend `target_backend` stamp behaved
/// differently under `--atomic` than under canonical `link`.
const EDGE_NATURAL_KEY_CONFLICT_SET: &str = "weight = excluded.weight, \
     updated_at = excluded.updated_at, \
     deleted_at = NULL, \
     metadata = excluded.metadata, \
     target_backend = excluded.target_backend";

/// A `WHERE`-clause fragment asserting the id bound to `id_param` (an SQL
/// placeholder like `?3`) resolves to a live edge endpoint — an
/// undeleted entity or note, an event (append-only, no `deleted_at`), or an
/// undeleted edge (the `annotates` relation's target may be any substrate,
/// including another edge; ADR-002/ADR-055). Shared by
/// [`edge_insert_guarded_by_endpoints_statement`] and
/// [`edge_endpoints_exist`] (#769) so the two "does this endpoint still
/// exist" probes — the guarded single-row insert and the guarded batch
/// pre-check — can never drift on which substrates count as valid
/// endpoints.
fn endpoint_exists_clause(id_param: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM entities WHERE id = {id_param} AND deleted_at IS NULL) \
         OR EXISTS (SELECT 1 FROM notes WHERE id = {id_param} AND deleted_at IS NULL) \
         OR EXISTS (SELECT 1 FROM events WHERE id = {id_param}) \
         OR EXISTS (SELECT 1 FROM graph_edges WHERE id = {id_param} AND deleted_at IS NULL)"
    )
}

/// The exact natural-key-upserting `INSERT ... ON CONFLICT` this store's
/// `upsert_edge` issues. Canonicalizes symmetric-relation endpoints first,
/// matching `upsert_edge`'s own call to `canonical_edge_endpoints`.
pub fn edge_upsert_statement(edge: &Edge) -> SqlStatement {
    let (source_id, target_id) =
        canonical_edge_endpoints(edge.relation, edge.source_id, edge.target_id);
    let metadata_str = edge
        .metadata
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());
    SqlStatement {
        sql: format!(
            "INSERT INTO graph_edges \
              (namespace, id, source_id, target_id, relation, weight, \
               created_at, updated_at, deleted_at, metadata, target_backend) \
              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
              ON CONFLICT(namespace, id) DO UPDATE SET \
                  source_id = excluded.source_id, \
                  target_id = excluded.target_id, \
                  relation = excluded.relation, \
                  {EDGE_NATURAL_KEY_CONFLICT_SET} \
              ON CONFLICT(namespace, source_id, target_id, relation) DO UPDATE SET \
                  {EDGE_NATURAL_KEY_CONFLICT_SET}"
        ),
        params: vec![
            SqlValue::Text(edge.namespace.clone()),
            SqlValue::Text(Uuid::from(edge.id).to_string()),
            SqlValue::Text(source_id.to_string()),
            SqlValue::Text(target_id.to_string()),
            SqlValue::Text(edge.relation.to_string()),
            SqlValue::Float(edge.weight),
            SqlValue::Integer(edge.created_at.timestamp_micros()),
            SqlValue::Integer(edge.updated_at.timestamp_micros()),
            match edge.deleted_at {
                Some(t) => SqlValue::Integer(t.timestamp_micros()),
                None => SqlValue::Null,
            },
            match metadata_str {
                Some(m) => SqlValue::Text(m),
                None => SqlValue::Null,
            },
            match &edge.target_backend {
                Some(b) => SqlValue::Text(b.clone()),
                None => SqlValue::Null,
            },
        ],
        label: Some("edge-upsert".to_string()),
    }
}

/// The atomic `link` op's variant of [`edge_upsert_statement`] (ADR-099
/// §B3). Shares the SAME
/// `EDGE_NATURAL_KEY_CONFLICT_SET` conflict-arm text — the two builders
/// cannot diverge on write behavior — but wraps the `INSERT` in a guarded
/// `SELECT ... WHERE EXISTS(...)` that re-probes both endpoints for
/// existence INSIDE the transaction, at commit time, rather than trusting
/// prepare-time validation alone.
///
/// This guard is atomic-`link`-specific, not a `edge_upsert_statement`
/// concern: `LinkPlan`'s own doc comment (`khive-runtime::atomic_plan`)
/// records why it must be commit-time, not prepare-time — a `link` op's
/// async prepare pass (`validate_edge_relation_endpoints`) can run and pass
/// BEFORE an earlier op in the SAME atomic unit (e.g. `delete(X, hard)`)
/// removes that very endpoint; only a commit-time, in-transaction guard
/// closes that intra-batch ordering hazard (ADR-099 acceptance criteria:
/// `[delete(X, hard), link(A, X)]` must fail, not silently create a
/// dangling edge). Canonical `link` has no equivalent need — it executes
/// and commits standalone, with no other op's write interleaved between its
/// own validation and its own write.
#[allow(clippy::too_many_arguments)]
pub fn edge_insert_guarded_by_endpoints_statement(
    namespace: &str,
    edge_id: Uuid,
    source_id: Uuid,
    target_id: Uuid,
    relation: EdgeRelation,
    weight: f64,
    now: i64,
    metadata: Option<&str>,
) -> SqlStatement {
    let src_exists = endpoint_exists_clause("?3");
    let tgt_exists = endpoint_exists_clause("?4");
    SqlStatement {
        sql: format!(
            "INSERT INTO graph_edges \
              (namespace, id, source_id, target_id, relation, weight, \
               created_at, updated_at, metadata) \
              SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8 \
              WHERE ({src_exists}) AND ({tgt_exists}) \
              ON CONFLICT(namespace, source_id, target_id, relation) DO UPDATE SET \
                  {EDGE_NATURAL_KEY_CONFLICT_SET}"
        ),
        params: vec![
            SqlValue::Text(namespace.to_string()),
            SqlValue::Text(edge_id.to_string()),
            SqlValue::Text(source_id.to_string()),
            SqlValue::Text(target_id.to_string()),
            SqlValue::Text(relation.as_str().to_string()),
            SqlValue::Float(weight),
            SqlValue::Integer(now),
            match metadata {
                Some(m) => SqlValue::Text(m.to_string()),
                None => SqlValue::Null,
            },
        ],
        label: Some("atomic-link-insert-edge-where-exists".to_string()),
    }
}

/// The exact soft-delete `UPDATE` this store's `delete_edge(Soft)` issues.
pub fn edge_soft_delete_statement(id: Uuid, now: i64) -> SqlStatement {
    SqlStatement {
        sql: "UPDATE graph_edges SET deleted_at = ?2, updated_at = ?2 \
              WHERE id = ?1 AND deleted_at IS NULL"
            .to_string(),
        params: vec![SqlValue::Text(id.to_string()), SqlValue::Integer(now)],
        label: Some("edge-delete-soft".to_string()),
    }
}

/// The exact hard-delete `DELETE` this store's `delete_edge(Hard)` issues.
pub fn edge_hard_delete_statement(id: Uuid) -> SqlStatement {
    SqlStatement {
        sql: "DELETE FROM graph_edges WHERE id = ?1".to_string(),
        params: vec![SqlValue::Text(id.to_string())],
        label: Some("edge-delete-hard".to_string()),
    }
}

/// The exact cascade `DELETE` this store's `purge_incident_edges` issues.
pub fn purge_incident_edges_statement(node_id: Uuid) -> SqlStatement {
    SqlStatement {
        sql: "DELETE FROM graph_edges WHERE source_id = ?1 OR target_id = ?1".to_string(),
        params: vec![SqlValue::Text(node_id.to_string())],
        label: Some("edge-purge-incident".to_string()),
    }
}

// ---------------------------------------------------------------------------
// Symmetric-relation update DML (ADR-099 B3 r6 second pass) — the SQL text
// `khive-runtime::operations::KhiveRuntime::update_edge_symmetric_dml` (the
// synchronous raw-connection commit-time path, run inside the writer-task/
// pool-mutex transaction) and ADR-099's atomic `prepare_update_edge` symmetric
// branch (the async plan-time path) both bind. `upsert_edge` cannot be used
// here: it resolves `ON CONFLICT(namespace, id)` first and cannot detect a
// natural-key collision at (namespace, source_id, target_id, relation) with a
// *different* id, which is exactly the case a symmetric-relation endpoint
// canonicalization can produce.
//
// The two call sites bind these against different parameter-passing
// mechanisms — `conn.execute`/`conn.query_row` with `rusqlite::params!` in the
// synchronous path (it must run inside an existing transaction on a borrowed
// `&rusqlite::Connection`, so it cannot go through the `SqlStatement`/
// `SqlValue` plan-shape khive-storage abstracts elsewhere) vs. `SqlValue`
// plan params for the async `PlanStatement` path — but the SQL TEXT itself
// (the `EDGE_SYMMETRIC_*_SQL` constants below) is the single source of truth
// for both, closing the class of drift that produced a hand-copied SQL
// literal silently diverging from canonical (ADR-099 §B3).
pub const EDGE_SYMMETRIC_CONFLICT_PROBE_SQL: &str = "SELECT id FROM graph_edges \
     WHERE namespace = ?1 AND source_id = ?2 AND target_id = ?3 \
     AND relation = ?4 AND id != ?5";

pub const EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL: &str =
    "DELETE FROM graph_edges WHERE namespace = ?1 AND id = ?2";

pub const EDGE_SYMMETRIC_REFRESH_CANONICAL_SQL: &str = "UPDATE graph_edges SET \
     weight = ?1, updated_at = ?2, deleted_at = NULL, \
     target_backend = ?3, metadata = ?4 \
     WHERE namespace = ?5 AND id = ?6";

pub const EDGE_SYMMETRIC_UPDATE_INPLACE_SQL: &str = "UPDATE graph_edges SET \
     source_id = ?1, target_id = ?2, relation = ?3, \
     weight = ?4, updated_at = ?5, metadata = ?6 \
     WHERE namespace = ?7 AND id = ?8";

/// Plan-shape builder for [`EDGE_SYMMETRIC_CONFLICT_PROBE_SQL`] — the
/// async prepare-time conflict probe.
pub fn edge_symmetric_conflict_probe_statement(
    namespace: &str,
    canon_src: Uuid,
    canon_tgt: Uuid,
    relation: EdgeRelation,
    exclude_id: Uuid,
) -> SqlStatement {
    SqlStatement {
        sql: EDGE_SYMMETRIC_CONFLICT_PROBE_SQL.to_string(),
        params: vec![
            SqlValue::Text(namespace.to_string()),
            SqlValue::Text(canon_src.to_string()),
            SqlValue::Text(canon_tgt.to_string()),
            SqlValue::Text(relation.to_string()),
            SqlValue::Text(exclude_id.to_string()),
        ],
        label: Some("edge-symmetric-conflict-probe".to_string()),
    }
}

/// Plan-shape builder for [`EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL`] —
/// case (b): a canonical row already exists, delete the requested row.
pub fn edge_symmetric_delete_noncanonical_statement(namespace: &str, id: Uuid) -> SqlStatement {
    SqlStatement {
        sql: EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL.to_string(),
        params: vec![
            SqlValue::Text(namespace.to_string()),
            SqlValue::Text(id.to_string()),
        ],
        label: Some("edge-symmetric-delete-noncanonical".to_string()),
    }
}

/// Plan-shape builder for [`EDGE_SYMMETRIC_REFRESH_CANONICAL_SQL`] —
/// case (b) continued: refresh the surviving canonical row.
#[allow(clippy::too_many_arguments)]
pub fn edge_symmetric_refresh_canonical_statement(
    namespace: &str,
    existing_id: Uuid,
    weight: f64,
    updated_at_micros: i64,
    target_backend: Option<&str>,
    metadata: Option<&str>,
) -> SqlStatement {
    SqlStatement {
        sql: EDGE_SYMMETRIC_REFRESH_CANONICAL_SQL.to_string(),
        params: vec![
            SqlValue::Float(weight),
            SqlValue::Integer(updated_at_micros),
            match target_backend {
                Some(b) => SqlValue::Text(b.to_string()),
                None => SqlValue::Null,
            },
            match metadata {
                Some(m) => SqlValue::Text(m.to_string()),
                None => SqlValue::Null,
            },
            SqlValue::Text(namespace.to_string()),
            SqlValue::Text(existing_id.to_string()),
        ],
        label: Some("edge-symmetric-refresh-canonical".to_string()),
    }
}

/// Plan-shape builder for [`EDGE_SYMMETRIC_UPDATE_INPLACE_SQL`] —
/// case (a): no conflict, update the requested row in place.
#[allow(clippy::too_many_arguments)]
pub fn edge_symmetric_update_inplace_statement(
    namespace: &str,
    id: Uuid,
    canon_src: Uuid,
    canon_tgt: Uuid,
    relation: EdgeRelation,
    weight: f64,
    updated_at_micros: i64,
    metadata: Option<&str>,
) -> SqlStatement {
    SqlStatement {
        sql: EDGE_SYMMETRIC_UPDATE_INPLACE_SQL.to_string(),
        params: vec![
            SqlValue::Text(canon_src.to_string()),
            SqlValue::Text(canon_tgt.to_string()),
            SqlValue::Text(relation.to_string()),
            SqlValue::Float(weight),
            SqlValue::Integer(updated_at_micros),
            match metadata {
                Some(m) => SqlValue::Text(m.to_string()),
                None => SqlValue::Null,
            },
            SqlValue::Text(namespace.to_string()),
            SqlValue::Text(id.to_string()),
        ],
        label: Some("edge-symmetric-update-inplace".to_string()),
    }
}

// ---------------------------------------------------------------------------
// Symmetric-relation update DML — atomic-only, commit-time self-guarding
// variant (ADR-099 §B3).
//
// The four builders above are still what canonical `update_edge_symmetric_dml`
// binds: it probes and branches synchronously INSIDE its own writer-task
// transaction, with no other op interleaved between its probe and its write,
// so its branch has no staleness exposure and is left untouched (control
// group — canonical's tests must stay green).
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
//    exists at the target natural key at THIS moment (guard: 0 or 1 rows).
// 2. [`edge_symmetric_refresh_or_update_inplace_statement`]: a single
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
//      BECAUSE a conflict existed — refresh that surviving canonical row.
//    These two arms are mutually exclusive and, together with statement 1's
//    own guard, jointly exhaustive: if the requested row no longer existed
//    when statement 1 ran (the same-unit race above), statement 1 affects 0
//    rows for a reason unrelated to conflict absorption, `id = ?2` no
//    longer matches anything (the row is gone), and the natural-key arm's
//    `changes() = 1` guard is false — so this statement affects ZERO rows
//    and the plan's `AffectedRowGuard::exactly(1)` on it fails the op,
//    aborting the whole atomic unit rather than silently mutating an
//    unrelated row. `target_backend` is updated only in the natural-key
//    (absorbed-conflict) arm — the same `changes() = 1` condition, via a
//    `CASE`, replicating [`EDGE_SYMMETRIC_UPDATE_INPLACE_SQL`]'s "leave
//    `target_backend` untouched" behavior for the in-place case with the
//    SAME statement that also replicates
//    [`EDGE_SYMMETRIC_REFRESH_CANONICAL_SQL`]'s explicit `target_backend`
//    set for the absorbed case.
//
// No probe, no branch, no read at all is needed to APPLY this pair. Which
// row this plan actually touched is derived post-commit by the caller via a
// fresh natural-key lookup (`khive-runtime::KhiveRuntime::list_edges`,
// filtered on the canonicalized endpoints/relation — the same mechanism the
// atomic `link` op's own result rendering already uses) — ADR-099 §B3
// removed the prior prepare-time advisory `target_id` probe entirely:
// a value computed before the
// SAME atomic unit's other ops have run is not a fact this plan can stand
// behind, so result rendering no longer trusts it.
pub fn edge_symmetric_delete_if_conflict_statement(
    namespace: &str,
    id: Uuid,
    canon_src: Uuid,
    canon_tgt: Uuid,
    relation: EdgeRelation,
) -> SqlStatement {
    SqlStatement {
        sql: "DELETE FROM graph_edges \
              WHERE namespace = ?1 AND id = ?2 \
                AND EXISTS ( \
                  SELECT 1 FROM graph_edges \
                  WHERE namespace = ?1 AND source_id = ?3 AND target_id = ?4 \
                    AND relation = ?5 AND id != ?2 \
                )"
        .to_string(),
        params: vec![
            SqlValue::Text(namespace.to_string()),
            SqlValue::Text(id.to_string()),
            SqlValue::Text(canon_src.to_string()),
            SqlValue::Text(canon_tgt.to_string()),
            SqlValue::Text(relation.to_string()),
        ],
        label: Some("edge-symmetric-delete-if-conflict".to_string()),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn edge_symmetric_refresh_or_update_inplace_statement(
    namespace: &str,
    id: Uuid,
    canon_src: Uuid,
    canon_tgt: Uuid,
    relation: EdgeRelation,
    weight: f64,
    updated_at_micros: i64,
    metadata: Option<&str>,
    target_backend: Option<&str>,
) -> SqlStatement {
    SqlStatement {
        sql: "UPDATE graph_edges SET \
              source_id = ?3, target_id = ?4, relation = ?5, \
              weight = ?6, updated_at = ?7, deleted_at = NULL, metadata = ?8, \
              target_backend = CASE WHEN changes() = 1 THEN ?9 ELSE target_backend END \
              WHERE namespace = ?1 \
                AND ( \
                  (id = ?2 AND changes() = 0) \
                  OR (source_id = ?3 AND target_id = ?4 AND relation = ?5 \
                      AND id != ?2 AND changes() = 1) \
                )"
        .to_string(),
        params: vec![
            SqlValue::Text(namespace.to_string()),
            SqlValue::Text(id.to_string()),
            SqlValue::Text(canon_src.to_string()),
            SqlValue::Text(canon_tgt.to_string()),
            SqlValue::Text(relation.to_string()),
            SqlValue::Float(weight),
            SqlValue::Integer(updated_at_micros),
            match metadata {
                Some(m) => SqlValue::Text(m.to_string()),
                None => SqlValue::Null,
            },
            match target_backend {
                Some(b) => SqlValue::Text(b.to_string()),
                None => SqlValue::Null,
            },
        ],
        label: Some("edge-symmetric-refresh-or-update-inplace".to_string()),
    }
}

/// A GraphStore backed by SQLite tables.
pub struct SqlGraphStore {
    pool: Arc<ConnectionPool>,
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
        // Best-effort opt-in (ADR-067 Component A, mirrors entity.rs slice 1
        // policy): a missing writer task degrades to the legacy pool-mutex /
        // standalone-connection path rather than failing construction.
        let writer_task = pool.writer_task_handle().ok().flatten();

        Self {
            pool,
            is_file_backed,
            namespace: namespace.into(),
            writer_task,
        }
    }

    fn open_standalone_writer(&self) -> Result<rusqlite::Connection, StorageError> {
        self.pool
            .open_standalone_writer()
            .map_err(|e| map_sqlite_err(e, "open_graph_writer"))
    }

    fn open_standalone_reader(&self) -> Result<rusqlite::Connection, StorageError> {
        self.pool
            .open_standalone_reader()
            .map_err(|e| map_sqlite_err(e, "open_graph_reader"))
    }

    /// Route a single-row write through the pool-wide `WriterTask` when
    /// `KHIVE_WRITE_QUEUE=1` and a handle is available; otherwise fall back
    /// to the legacy standalone-connection / pool-mutex path (ADR-067
    /// Component A, Fork C slice 2). `f` must be DML-only. See
    /// `crates/khive-db/docs/api/graph.md` for the per-caller routing rules.
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

        if self.is_file_backed {
            let conn = self.open_standalone_writer()?;
            tokio::task::spawn_blocking(move || f(&conn).map_err(|e| map_err(e, op)))
                .await
                .map_err(|e| StorageError::driver(StorageCapability::Graph, op, e))?
        } else {
            let pool = Arc::clone(&self.pool);
            tokio::task::spawn_blocking(move || {
                let guard = pool.try_writer().map_err(|e| map_sqlite_err(e, op))?;
                f(guard.conn()).map_err(|e| map_err(e, op))
            })
            .await
            .map_err(|e| StorageError::driver(StorageCapability::Graph, op, e))?
        }
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
                .map_err(|e| StorageError::driver(StorageCapability::Graph, op, e))?
        } else {
            let pool = Arc::clone(&self.pool);
            tokio::task::spawn_blocking(move || {
                let guard = pool.reader().map_err(|e| map_sqlite_err(e, op))?;
                f(guard.conn()).map_err(|e| map_err(e, op))
            })
            .await
            .map_err(|e| StorageError::driver(StorageCapability::Graph, op, e))?
        }
    }
}

// =============================================================================
// Helpers
// =============================================================================

fn read_edge(row: &rusqlite::Row<'_>) -> Result<Edge, rusqlite::Error> {
    let namespace: String = row.get(0)?;
    let id_str: String = row.get(1)?;
    let source_str: String = row.get(2)?;
    let target_str: String = row.get(3)?;
    let relation_str: String = row.get(4)?;
    let weight: f64 = row.get(5)?;
    let created_micros: i64 = row.get(6)?;
    let updated_micros: i64 = row.get(7)?;
    let deleted_micros: Option<i64> = row.get(8)?;
    let metadata_str: Option<String> = row.get(9)?;
    let target_backend: Option<String> = row.get(10)?;

    let id = parse_uuid(&id_str)?;
    let source_id = parse_uuid(&source_str)?;
    let target_id = parse_uuid(&target_str)?;
    let created_at = micros_to_datetime(created_micros);
    let relation = relation_str.parse::<EdgeRelation>().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let metadata = match metadata_str {
        Some(s) => {
            let v = serde_json::from_str(&s).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    9,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            Some(v)
        }
        None => None,
    };

    Ok(Edge {
        id: id.into(),
        namespace,
        source_id,
        target_id,
        relation,
        weight,
        created_at,
        updated_at: micros_to_datetime(updated_micros),
        deleted_at: deleted_micros.map(micros_to_datetime),
        metadata,
        target_backend,
    })
}

fn parse_uuid(s: &str) -> Result<Uuid, rusqlite::Error> {
    Uuid::parse_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

/// Build the `relation IN (...)` / `weight >= ?` `WHERE`-extra clause and the
/// `LIMIT` clause shared by `neighbors` and `neighbors_both_directions` —
/// both filter and cap identically, differing only in which direction(s) the
/// base `SELECT`s cover. `start_param_idx` is the next free `?N` placeholder
/// (both callers bind `namespace` and `node_id` as `?1`/`?2` first).
fn neighbor_extra_clause(
    query: &NeighborQuery,
    start_param_idx: usize,
) -> (String, String, Vec<Box<dyn rusqlite::types::ToSql>>) {
    let mut conditions: Vec<String> = Vec::new();
    let mut extra_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut param_idx = start_param_idx;

    if let Some(ref rels) = query.relations {
        if !rels.is_empty() {
            let placeholders: Vec<String> = rels
                .iter()
                .map(|r| {
                    extra_params.push(Box::new(r.to_string()));
                    let p = format!("?{}", param_idx);
                    param_idx += 1;
                    p
                })
                .collect();
            conditions.push(format!("relation IN ({})", placeholders.join(",")));
        }
    }

    if let Some(min_w) = query.min_weight {
        extra_params.push(Box::new(min_w));
        conditions.push(format!("weight >= ?{}", param_idx));
        param_idx += 1;
    }

    let where_extra = if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    };

    let limit_clause = if let Some(lim) = query.limit {
        extra_params.push(Box::new(lim as i64));
        format!(" LIMIT ?{}", param_idx)
    } else {
        String::new()
    };

    (where_extra, limit_clause, extra_params)
}

// Test-only counter of storage-level neighbor SELECT executions (`neighbors`
// and `neighbors_both_directions` each issue exactly one `graph_edges`
// query per call). Lets tests assert the query-count halving a
// `Direction::Both` caller gets from `neighbors_both_directions` vs the old
// pattern of two separate `neighbors` calls (ADR-089 context-verb
// optimization). Gated out of release builds — no counter overhead on the
// hot path in production.
#[cfg(test)]
static NEIGHBOR_SELECT_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
fn count_neighbor_select() {
    NEIGHBOR_SELECT_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(not(test))]
fn count_neighbor_select() {}

#[cfg(test)]
pub(crate) fn reset_neighbor_select_count() {
    NEIGHBOR_SELECT_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn neighbor_select_count() -> usize {
    NEIGHBOR_SELECT_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

fn micros_to_datetime(micros: i64) -> DateTime<Utc> {
    Utc.timestamp_micros(micros)
        .single()
        .unwrap_or_else(Utc::now)
}

fn build_edge_filter_sql(
    namespace: &str,
    filter: &EdgeFilter,
) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>) {
    let mut conditions: Vec<String> = vec![
        "namespace = ?1".to_string(),
        "deleted_at IS NULL".to_string(),
    ];
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(namespace.to_string())];

    if !filter.ids.is_empty() {
        let placeholders: Vec<String> = filter
            .ids
            .iter()
            .map(|id| {
                params.push(Box::new(id.to_string()));
                format!("?{}", params.len())
            })
            .collect();
        conditions.push(format!("id IN ({})", placeholders.join(",")));
    }

    if !filter.source_ids.is_empty() {
        let placeholders: Vec<String> = filter
            .source_ids
            .iter()
            .map(|id| {
                params.push(Box::new(id.to_string()));
                format!("?{}", params.len())
            })
            .collect();
        conditions.push(format!("source_id IN ({})", placeholders.join(",")));
    }

    if !filter.target_ids.is_empty() {
        let placeholders: Vec<String> = filter
            .target_ids
            .iter()
            .map(|id| {
                params.push(Box::new(id.to_string()));
                format!("?{}", params.len())
            })
            .collect();
        conditions.push(format!("target_id IN ({})", placeholders.join(",")));
    }

    if !filter.relations.is_empty() {
        let placeholders: Vec<String> = filter
            .relations
            .iter()
            .map(|r| {
                params.push(Box::new(r.to_string()));
                format!("?{}", params.len())
            })
            .collect();
        conditions.push(format!("relation IN ({})", placeholders.join(",")));
    }

    if let Some(min_w) = filter.min_weight {
        params.push(Box::new(min_w));
        conditions.push(format!("weight >= ?{}", params.len()));
    }

    if let Some(max_w) = filter.max_weight {
        params.push(Box::new(max_w));
        conditions.push(format!("weight <= ?{}", params.len()));
    }

    if let Some(ref time_range) = filter.created_at {
        if let Some(start) = time_range.start {
            params.push(Box::new(start.timestamp_micros()));
            conditions.push(format!("created_at >= ?{}", params.len()));
        }
        if let Some(end) = time_range.end {
            params.push(Box::new(end.timestamp_micros()));
            conditions.push(format!("created_at < ?{}", params.len()));
        }
    }

    let clause = format!(" WHERE {}", conditions.join(" AND "));
    (clause, params)
}

fn edge_sort_col(field: &EdgeSortField) -> &'static str {
    match field {
        EdgeSortField::CreatedAt => "created_at",
        EdgeSortField::Weight => "weight",
        EdgeSortField::Relation => "relation",
    }
}

// =============================================================================
// GraphStore implementation
// =============================================================================

/// Canonical endpoint order for symmetric relations (F012).
///
/// For `competes_with` and `composed_with`, ensures `source_uuid < target_uuid`
/// so A→B and B→A collapse to a single canonical row in storage.
fn canonical_edge_endpoints(
    relation: EdgeRelation,
    source_id: Uuid,
    target_id: Uuid,
) -> (Uuid, Uuid) {
    if relation.is_symmetric() && target_id < source_id {
        (target_id, source_id)
    } else {
        (source_id, target_id)
    }
}

/// DML-only batch upsert loop shared by both the legacy (flag-off) and
/// WriterTask-routed (flag-on) `upsert_edges` paths. Issues no
/// `BEGIN`/`COMMIT`/`ROLLBACK` itself — the caller owns the transaction.
/// All-or-nothing: the first row failure returns `Err` immediately. See
/// `crates/khive-db/docs/api/graph.md` for why this shares
/// [`edge_upsert_statement`]'s conflict-arm builder rather than a
/// second copy.
fn batch_upsert_edges(
    conn: &rusqlite::Connection,
    edges: &[Edge],
    attempted: u64,
) -> Result<BatchWriteSummary, rusqlite::Error> {
    let mut affected = 0u64;

    for edge in edges {
        let statement = edge_upsert_statement(edge);
        let mut stmt = conn.prepare(&statement.sql)?;
        bind_params(&mut stmt, &statement.params)?;
        stmt.raw_execute()?;
        affected += 1;
    }

    Ok(BatchWriteSummary {
        attempted,
        affected,
        failed: 0,
        first_error: String::new(),
    })
}

/// Standalone existence probe for both endpoints of a would-be edge (#769),
/// matching the `WHERE EXISTS(...)` shape
/// [`edge_insert_guarded_by_endpoints_statement`] embeds in its own guarded
/// `INSERT`. Returns per-endpoint existence (not a single AND'd bool) so
/// callers can report exactly which side was missing. See
/// `crates/khive-db/docs/api/graph.md` for its two call sites and why both
/// need in-transaction, not reconstructed-after-the-fact, results.
fn edge_endpoints_exist(
    conn: &rusqlite::Connection,
    source_id: Uuid,
    target_id: Uuid,
) -> Result<MissingEndpoints, rusqlite::Error> {
    let src_exists = endpoint_exists_clause("?1");
    let tgt_exists = endpoint_exists_clause("?2");
    let sql = format!("SELECT ({src_exists}), ({tgt_exists})");
    conn.query_row(
        &sql,
        rusqlite::params![source_id.to_string(), target_id.to_string()],
        |row| {
            let src_exists: bool = row.get(0)?;
            let tgt_exists: bool = row.get(1)?;
            Ok(MissingEndpoints {
                source: !src_exists,
                target: !tgt_exists,
            })
        },
    )
}

/// DML-only guarded single-row insert shared by both the legacy (flag-off)
/// and WriterTask-routed (flag-on) `upsert_edge_guarded` paths.
///
/// Runs the guarded `INSERT` and, if it was refused, the missing-endpoint
/// probe on the SAME connection with no gap for another writer to intervene
/// between them, PROVIDED the caller holds the connection under a single
/// write-locked transaction (either the WriterTask's own `BEGIN IMMEDIATE`,
/// or an explicit one the flag-off caller opens around this call: the
/// singleton fallback previously ran the insert and the
/// probe as two separate autocommit statements).
fn edge_insert_guarded(
    conn: &rusqlite::Connection,
    statement: &SqlStatement,
    source_id: Uuid,
    target_id: Uuid,
) -> Result<GuardedWriteOutcome, rusqlite::Error> {
    let mut stmt = conn.prepare(&statement.sql)?;
    bind_params(&mut stmt, &statement.params)?;
    if stmt.raw_execute()? > 0 {
        return Ok(GuardedWriteOutcome::Written);
    }
    // Test-only observation point for the exact insert-to-probe seam this
    // function's doc comment describes: a no-op in every non-test build,
    // and a no-op in test builds unless a test has installed a barrier for
    // this precise (source_id, target_id) pair (see
    // `tests::insert_probe_seam` in graph_tests.rs). Lets the
    // atomicity regression test force a racer to run at this seam instead
    // of guessing at it with sleeps.
    #[cfg(test)]
    tests::insert_probe_seam::hook((source_id, target_id));
    let missing = edge_endpoints_exist(conn, source_id, target_id)?;
    Ok(GuardedWriteOutcome::Refused(missing))
}

/// DML-only guarded batch upsert loop shared by both the legacy (flag-off)
/// and WriterTask-routed (flag-on) `upsert_edges_guarded` paths, mirroring
/// [`batch_upsert_edges`]'s split. Pre-checks every edge's endpoints with
/// [`edge_endpoints_exist`] BEFORE issuing any `INSERT` — if any endpoint is
/// missing, returns immediately with `affected: 0` and no writes at all
/// (#769), so the caller's transaction has nothing to roll back. See
/// `crates/khive-db/docs/api/graph.md`.
fn batch_upsert_edges_guarded(
    conn: &rusqlite::Connection,
    edges: &[Edge],
    attempted: u64,
) -> Result<GuardedBatchOutcome, rusqlite::Error> {
    for (index, edge) in edges.iter().enumerate() {
        let (source_id, target_id) =
            canonical_edge_endpoints(edge.relation, edge.source_id, edge.target_id);
        let missing = edge_endpoints_exist(conn, source_id, target_id)?;
        if missing.any() {
            return Ok(GuardedBatchOutcome {
                summary: BatchWriteSummary {
                    attempted,
                    affected: 0,
                    failed: attempted,
                    first_error: format!(
                        "batch entry {index}: edge endpoint no longer exists at write time: source {source_id} or target {target_id}"
                    ),
                },
                refused: Some(GuardedBatchRefusal {
                    entry_index: index,
                    missing,
                }),
            });
        }
    }

    let mut affected = 0u64;
    for edge in edges {
        let statement = edge_upsert_statement(edge);
        let mut stmt = conn.prepare(&statement.sql)?;
        bind_params(&mut stmt, &statement.params)?;
        stmt.raw_execute()?;
        affected += 1;
    }

    Ok(GuardedBatchOutcome {
        summary: BatchWriteSummary {
            attempted,
            affected,
            failed: 0,
            first_error: String::new(),
        },
        refused: None,
    })
}

#[async_trait]
impl GraphStore for SqlGraphStore {
    async fn upsert_edge(&self, edge: Edge) -> Result<(), StorageError> {
        let statement = edge_upsert_statement(&edge);
        self.with_writer("upsert_edge", move |conn| {
            let mut stmt = conn.prepare(&statement.sql)?;
            bind_params(&mut stmt, &statement.params)?;
            stmt.raw_execute()?;
            Ok(())
        })
        .await
    }

    async fn upsert_edges(&self, edges: Vec<Edge>) -> Result<BatchWriteSummary, StorageError> {
        let attempted = edges.len() as u64;

        // ADR-067 Component A: when the write queue is enabled, route
        // through the pool-wide WriterTask. DML-only closure — no BEGIN
        // IMMEDIATE/COMMIT/ROLLBACK here, since the WriterTask's run loop
        // owns the transaction (a bare BEGIN IMMEDIATE here would violate
        // SQLite's nested-transaction rule).
        if let Some(writer_task) = &self.writer_task {
            return writer_task
                .send(move |conn| {
                    batch_upsert_edges(conn, &edges, attempted)
                        .map_err(|e| map_err(e, "upsert_edges"))
                })
                .await;
        }

        // Flag-off (default) path: byte-for-byte unchanged from pre-ADR-067
        // behavior — the closure owns its own BEGIN IMMEDIATE/COMMIT/ROLLBACK
        // via the pool-mutex/standalone writer.
        self.with_writer("upsert_edges", move |conn| {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let _tx_handle =
                khive_storage::tx_registry::register(Some("graph_upsert_edges".to_string()));

            let summary = match batch_upsert_edges(conn, &edges, attempted) {
                Ok(summary) => summary,
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(e);
                }
            };

            if let Err(e) = conn.execute_batch("COMMIT") {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
            Ok(summary)
        })
        .await
    }

    async fn upsert_edge_guarded(&self, edge: Edge) -> Result<GuardedWriteOutcome, StorageError> {
        let (source_id, target_id) =
            canonical_edge_endpoints(edge.relation, edge.source_id, edge.target_id);
        let metadata_str = edge
            .metadata
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        let statement = edge_insert_guarded_by_endpoints_statement(
            &edge.namespace,
            Uuid::from(edge.id),
            source_id,
            target_id,
            edge.relation,
            edge.weight,
            edge.created_at.timestamp_micros(),
            metadata_str.as_deref(),
        );

        // Same WriterTask routing as `upsert_edges_guarded` — the
        // WriterTask's run loop owns its own `BEGIN IMMEDIATE`, so the
        // insert and the missing-endpoint probe below already run inside
        // one write-locked transaction; a bare `BEGIN IMMEDIATE` here would
        // violate SQLite's nested-transaction rule.
        if let Some(writer_task) = &self.writer_task {
            return writer_task
                .send(move |conn| {
                    edge_insert_guarded(conn, &statement, source_id, target_id)
                        .map_err(|e| map_err(e, "upsert_edge_guarded"))
                })
                .await;
        }

        // Flag-off (singleton) path: wrap the insert and the refused-probe
        // in one explicit transaction so nothing can change an endpoint
        // between them (this fallback previously
        // ran the two as separate autocommit statements on the standalone
        // writer connection).
        self.with_writer("upsert_edge_guarded", move |conn| {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let _tx_handle =
                khive_storage::tx_registry::register(Some("graph_upsert_edge_guarded".to_string()));

            let outcome = match edge_insert_guarded(conn, &statement, source_id, target_id) {
                Ok(outcome) => outcome,
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(e);
                }
            };

            if let Err(e) = conn.execute_batch("COMMIT") {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
            Ok(outcome)
        })
        .await
    }

    async fn upsert_edges_guarded(
        &self,
        edges: Vec<Edge>,
    ) -> Result<GuardedBatchOutcome, StorageError> {
        let attempted = edges.len() as u64;

        // Same WriterTask routing as `upsert_edges` — the guard's pre-check
        // runs inside the WriterTask's own `BEGIN IMMEDIATE`, so a missing
        // endpoint is caught before any `INSERT` in this batch runs at all.
        if let Some(writer_task) = &self.writer_task {
            return writer_task
                .send(move |conn| {
                    batch_upsert_edges_guarded(conn, &edges, attempted)
                        .map_err(|e| map_err(e, "upsert_edges_guarded"))
                })
                .await;
        }

        self.with_writer("upsert_edges_guarded", move |conn| {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let _tx_handle = khive_storage::tx_registry::register(Some(
                "graph_upsert_edges_guarded".to_string(),
            ));

            let summary = match batch_upsert_edges_guarded(conn, &edges, attempted) {
                Ok(summary) => summary,
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(e);
                }
            };

            if let Err(e) = conn.execute_batch("COMMIT") {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
            Ok(summary)
        })
        .await
    }

    async fn get_edge(&self, id: LinkId) -> Result<Option<Edge>, StorageError> {
        let id_str = Uuid::from(id).to_string();

        self.with_reader("get_edge", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT namespace, id, source_id, target_id, relation, weight, \
                        created_at, updated_at, deleted_at, metadata, target_backend \
                 FROM graph_edges WHERE id = ?1 AND deleted_at IS NULL",
            )?;
            let mut rows = stmt.query(rusqlite::params![id_str])?;
            match rows.next()? {
                Some(row) => Ok(Some(read_edge(row)?)),
                None => Ok(None),
            }
        })
        .await
    }

    async fn get_edge_including_deleted(&self, id: LinkId) -> Result<Option<Edge>, StorageError> {
        let id_str = Uuid::from(id).to_string();

        self.with_reader("get_edge_including_deleted", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT namespace, id, source_id, target_id, relation, weight, \
                        created_at, updated_at, deleted_at, metadata, target_backend \
                 FROM graph_edges WHERE id = ?1",
            )?;
            let mut rows = stmt.query(rusqlite::params![id_str])?;
            match rows.next()? {
                Some(row) => Ok(Some(read_edge(row)?)),
                None => Ok(None),
            }
        })
        .await
    }

    async fn get_edges(&self, ids: &[LinkId]) -> Result<Vec<Edge>, StorageError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // SQLite SQLITE_MAX_VARIABLE_NUMBER defaults to 999; chunk at 900 to stay safe.
        const CHUNK: usize = 900;
        let id_strs: Vec<String> = ids.iter().map(|id| Uuid::from(*id).to_string()).collect();

        let mut result: Vec<Edge> = Vec::with_capacity(ids.len());
        for chunk in id_strs.chunks(CHUNK) {
            let chunk_owned: Vec<String> = chunk.to_vec();
            let edges = self
                .with_reader("get_edges", move |conn| {
                    let placeholders: Vec<String> =
                        (1..=chunk_owned.len()).map(|i| format!("?{}", i)).collect();
                    let sql = format!(
                        "SELECT namespace, id, source_id, target_id, relation, weight, \
                                created_at, updated_at, deleted_at, metadata, target_backend \
                         FROM graph_edges WHERE id IN ({}) AND deleted_at IS NULL",
                        placeholders.join(",")
                    );
                    let mut stmt = conn.prepare(&sql)?;
                    let params: Vec<&dyn rusqlite::types::ToSql> = chunk_owned
                        .iter()
                        .map(|s| s as &dyn rusqlite::types::ToSql)
                        .collect();
                    let rows = stmt.query_map(params.as_slice(), read_edge)?;
                    let mut edges = Vec::new();
                    for row in rows {
                        edges.push(row?);
                    }
                    Ok(edges)
                })
                .await?;
            result.extend(edges);
        }
        Ok(result)
    }

    async fn batch_neighbors(
        &self,
        sources: &[Uuid],
        query: NeighborQuery,
    ) -> Result<Vec<(Uuid, NeighborHit)>, StorageError> {
        use khive_storage::types::Direction;

        if sources.is_empty() {
            return Ok(Vec::new());
        }
        let mut seen_sources = HashSet::with_capacity(sources.len());
        let unique_sources: Vec<Uuid> = sources
            .iter()
            .copied()
            .filter(|source| seen_sources.insert(*source))
            .collect();
        const CHUNK_SIZE: usize = 880;

        let namespace = self.namespace.clone();
        let mut result: Vec<(Uuid, NeighborHit)> = Vec::new();

        for chunk in unique_sources.chunks(CHUNK_SIZE) {
            let chunk_owned: Vec<Uuid> = chunk.to_vec();
            let query_clone = query.clone();
            let ns = namespace.clone();

            let pairs = self
                .with_reader("batch_neighbors", move |conn| {
                    let src_strs: Vec<String> = chunk_owned.iter().map(|u| u.to_string()).collect();

                    let sources_json = serde_json::to_string(&src_strs).map_err(|error| {
                        rusqlite::Error::ToSqlConversionFailure(Box::new(error))
                    })?;

                    let build_inner_sql =
                        |direction_out: bool,
                         q: &NeighborQuery|
                         -> (String, Vec<String>, Option<f64>) {
                            let (filter_col, node_col) = if direction_out {
                                ("source_id", "target_id")
                            } else {
                                ("target_id", "source_id")
                            };

                            let mut rel_params: Vec<String> = Vec::new();
                            let mut conditions: Vec<String> = Vec::new();
                            let mut param_idx = 3;

                            if let Some(ref rels) = q.relations {
                                if !rels.is_empty() {
                                    let ps: Vec<String> = rels
                                        .iter()
                                        .map(|r| {
                                            rel_params.push(r.to_string());
                                            let p = format!("?{param_idx}");
                                            param_idx += 1;
                                            p
                                        })
                                        .collect();
                                    conditions
                                        .push(format!("edges.relation IN ({})", ps.join(",")));
                                }
                            }

                            // min_weight is returned separately so it can be added to
                            // all_params AFTER the rel_params block, at the right index.
                            let min_weight_val = if let Some(min_w) = q.min_weight {
                                conditions.push(format!("edges.weight >= ?{param_idx}"));
                                Some(min_w)
                            } else {
                                None
                            };

                            let where_extra = if conditions.is_empty() {
                                String::new()
                            } else {
                                format!(" AND {}", conditions.join(" AND "))
                            };

                            let sql = format!(
                                "SELECT requested.origin_id, edges.{node_col} AS node_id, \
                                 edges.id AS edge_id, edges.relation, edges.weight \
                                 FROM requested CROSS JOIN graph_edges AS edges \
                                   ON edges.{filter_col} = requested.origin_id \
                                 WHERE edges.namespace = ?1 \
                                   AND edges.deleted_at IS NULL{where_extra}",
                            );
                            (sql, rel_params, min_weight_val)
                        };

                    let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                    all_params.push(Box::new(ns.to_string()));
                    all_params.push(Box::new(sources_json));

                    let (combined_inner, rel_params, min_weight_val) = match query_clone.direction {
                        Direction::Out => build_inner_sql(true, &query_clone),
                        Direction::In => build_inner_sql(false, &query_clone),
                        Direction::Both => {
                            let (out_sql, rel_params, min_weight_val) =
                                build_inner_sql(true, &query_clone);
                            let (in_sql, _, _) = build_inner_sql(false, &query_clone);
                            (
                                format!("{out_sql} UNION ALL {in_sql}"),
                                rel_params,
                                min_weight_val,
                            )
                        }
                    };

                    for relation in rel_params {
                        all_params.push(Box::new(relation));
                    }
                    if let Some(min_weight) = min_weight_val {
                        all_params.push(Box::new(min_weight));
                    }
                    let limit_param_idx = all_params.len() + 1;

                    // Wrap combined inner with per-source ROW_NUMBER limit if needed.
                    //
                    // Deterministic weight-descending order, tie-broken by node_id
                    // ascending, applied INSIDE the window's ORDER BY — otherwise a
                    // per-origin cap can silently drop high-weight neighbors in favor
                    // of arbitrary SQLite row order (mirrors neighbors(), ADR-089
                    // context-verb review; issue #589).
                    let full_sql = if let Some(lim) = query_clone.limit {
                        all_params.push(Box::new(lim as i64));
                        format!(
                            "WITH requested(origin_id) AS (\
                               SELECT value FROM json_each(?2)\
                             ) SELECT origin_id, node_id, edge_id, relation, weight \
                             FROM (SELECT *, ROW_NUMBER() OVER (PARTITION BY origin_id \
                                   ORDER BY weight DESC, node_id ASC) AS rn \
                                   FROM ({combined_inner})) WHERE rn <= ?{limit_param_idx}",
                        )
                    } else {
                        format!(
                            "WITH requested(origin_id) AS (\
                               SELECT value FROM json_each(?2)\
                             ) SELECT origin_id, node_id, edge_id, relation, weight \
                             FROM ({combined_inner})",
                        )
                    };

                    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                        all_params.iter().map(|p| p.as_ref()).collect();

                    let mut stmt = conn.prepare(&full_sql)?;
                    let rows = stmt.query_map(param_refs.as_slice(), |row| {
                        let origin_str: String = row.get(0)?;
                        let nid_str: String = row.get(1)?;
                        let eid_str: String = row.get(2)?;
                        let relation_str: String = row.get(3)?;
                        let weight: f64 = row.get(4)?;
                        Ok((origin_str, nid_str, eid_str, relation_str, weight))
                    })?;

                    let mut pairs = Vec::new();
                    for row in rows {
                        let (origin_str, nid_str, eid_str, relation_str, weight) = row?;
                        let origin = parse_uuid(&origin_str)?;
                        let node_id = parse_uuid(&nid_str)?;
                        let edge_id = parse_uuid(&eid_str)?;
                        let relation = relation_str.parse::<EdgeRelation>().map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                3,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })?;
                        pairs.push((
                            origin,
                            NeighborHit {
                                node_id,
                                edge_id,
                                relation,
                                weight,
                                name: None,
                                kind: None,
                                entity_type: None,
                            },
                        ));
                    }
                    Ok(pairs)
                })
                .await?;
            result.extend(pairs);
        }

        let requested: HashSet<Uuid> = unique_sources.iter().copied().collect();
        let mut grouped: HashMap<Uuid, Vec<NeighborHit>> =
            HashMap::with_capacity(unique_sources.len());
        for (origin, hit) in result {
            if !requested.contains(&origin) {
                return Err(StorageError::Internal(format!(
                    "batch_neighbors returned unrequested origin {origin}"
                )));
            }
            grouped.entry(origin).or_default().push(hit);
        }

        for hits in grouped.values_mut() {
            hits.sort_by(|a, b| {
                b.weight
                    .partial_cmp(&a.weight)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.node_id.cmp(&b.node_id))
                    .then(a.edge_id.cmp(&b.edge_id))
            });
        }

        let mut ordered = Vec::new();
        for &source in sources {
            if let Some(hits) = grouped.get(&source) {
                ordered.extend(hits.iter().cloned().map(|hit| (source, hit)));
            }
        }
        Ok(ordered)
    }

    async fn delete_edge(&self, id: LinkId, mode: DeleteMode) -> Result<bool, StorageError> {
        let id = Uuid::from(id);
        let statement = match mode {
            DeleteMode::Soft => {
                edge_soft_delete_statement(id, chrono::Utc::now().timestamp_micros())
            }
            DeleteMode::Hard => edge_hard_delete_statement(id),
        };
        self.with_writer("delete_edge", move |conn| {
            let mut stmt = conn.prepare(&statement.sql)?;
            bind_params(&mut stmt, &statement.params)?;
            Ok(stmt.raw_execute()? > 0)
        })
        .await
    }

    async fn query_edges(
        &self,
        filter: EdgeFilter,
        sort: Vec<SortOrder<EdgeSortField>>,
        page: PageRequest,
    ) -> Result<Page<Edge>, StorageError> {
        let namespace = self.namespace.clone();
        let limit_i64 = i64::from(page.limit);
        let offset_i64 = i64::try_from(page.offset).map_err(|_| StorageError::InvalidInput {
            capability: StorageCapability::Graph,
            operation: "query_edges".into(),
            message: format!(
                "PageRequest: offset must be <= i64::MAX, got {}",
                page.offset
            ),
        })?;
        self.with_reader("query_edges", move |conn| {
            let (where_clause, filter_params) = build_edge_filter_sql(&namespace, &filter);

            let count_sql = format!("SELECT COUNT(*) FROM graph_edges{}", where_clause);
            let total: i64 = {
                let mut stmt = conn.prepare(&count_sql)?;
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    filter_params.iter().map(|p| p.as_ref()).collect();
                stmt.query_row(param_refs.as_slice(), |row| row.get(0))?
            };

            let order_clause = if sort.is_empty() {
                " ORDER BY created_at DESC".to_string()
            } else {
                let parts: Vec<String> = sort
                    .iter()
                    .map(|s| {
                        let dir = match s.direction {
                            SortDirection::Asc => "ASC",
                            SortDirection::Desc => "DESC",
                        };
                        format!("{} {}", edge_sort_col(&s.field), dir)
                    })
                    .collect();
                format!(" ORDER BY {}", parts.join(", "))
            };

            let (_, data_filter_params) = build_edge_filter_sql(&namespace, &filter);
            let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> = data_filter_params;
            all_params.push(Box::new(limit_i64));
            all_params.push(Box::new(offset_i64));

            let limit_idx = all_params.len() - 1;
            let offset_idx = all_params.len();

            let data_sql = format!(
                "SELECT namespace, id, source_id, target_id, relation, weight, \
                        created_at, updated_at, deleted_at, metadata, target_backend \
                 FROM graph_edges{}{} LIMIT ?{} OFFSET ?{}",
                where_clause, order_clause, limit_idx, offset_idx,
            );

            let mut stmt = conn.prepare(&data_sql)?;
            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                all_params.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(param_refs.as_slice(), read_edge)?;

            let mut items = Vec::new();
            for row in rows {
                items.push(row?);
            }

            Ok(Page {
                items,
                total: Some(total as u64),
            })
        })
        .await
    }

    async fn count_edges(&self, filter: EdgeFilter) -> Result<u64, StorageError> {
        let namespace = self.namespace.clone();
        self.with_reader("count_edges", move |conn| {
            let (where_clause, params) = build_edge_filter_sql(&namespace, &filter);
            let sql = format!("SELECT COUNT(*) FROM graph_edges{}", where_clause);
            let mut stmt = conn.prepare(&sql)?;
            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(|p| p.as_ref()).collect();
            let count: i64 = stmt.query_row(param_refs.as_slice(), |row| row.get(0))?;
            Ok(count as u64)
        })
        .await
    }

    async fn count_edges_by_relation(&self) -> Result<Vec<(EdgeRelation, u64)>, StorageError> {
        let namespace = self.namespace.clone();
        self.with_reader("count_edges_by_relation", move |conn| {
            let sql = "SELECT relation, COUNT(*) FROM graph_edges \
                       WHERE namespace = ?1 AND deleted_at IS NULL \
                       GROUP BY relation";
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt.query_map([&namespace], |row| {
                let relation_str: String = row.get(0)?;
                let count: i64 = row.get(1)?;
                Ok((relation_str, count))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (relation_str, count) = row?;
                let relation = relation_str.parse::<EdgeRelation>().map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
                out.push((relation, count as u64));
            }
            Ok(out)
        })
        .await
    }

    async fn query_edges_after(
        &self,
        filter: EdgeFilter,
        after: Option<Uuid>,
        limit: u32,
    ) -> Result<EdgeSeekPage, StorageError> {
        let namespace = self.namespace.clone();
        let limit_usize = limit as usize;
        let probe_limit_i64 = i64::from(limit) + 1;
        self.with_reader("query_edges_after", move |conn| {
            let (mut where_clause, mut params) = build_edge_filter_sql(&namespace, &filter);
            if let Some(cursor) = after {
                params.push(Box::new(cursor.to_string()));
                where_clause.push_str(&format!(" AND id > ?{}", params.len()));
            }
            params.push(Box::new(probe_limit_i64));
            let limit_idx = params.len();

            // `where_clause` always pins `namespace = ?1`; adding `id > ?N` here
            // keeps the predicate a range scan against the implicit unique index
            // backing `PRIMARY KEY (namespace, id)` — equality on the leading
            // column plus a range on the trailing one, with `ORDER BY id ASC`
            // matching the index order, so SQLite seeks instead of scanning.
            let data_sql = format!(
                "SELECT namespace, id, source_id, target_id, relation, weight, \
                        created_at, updated_at, deleted_at, metadata, target_backend \
                 FROM graph_edges{} ORDER BY id ASC LIMIT ?{}",
                where_clause, limit_idx,
            );

            let mut stmt = conn.prepare(&data_sql)?;
            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(param_refs.as_slice(), read_edge)?;

            let mut items = Vec::new();
            for row in rows {
                items.push(row?);
            }
            let has_more = items.len() > limit_usize;
            if has_more {
                items.truncate(limit_usize);
            }
            let next_after = if has_more {
                items.last().map(|e| Uuid::from(e.id))
            } else {
                None
            };

            Ok(EdgeSeekPage { items, next_after })
        })
        .await
    }

    async fn neighbors(
        &self,
        node_id: Uuid,
        query: NeighborQuery,
    ) -> Result<Vec<NeighborHit>, StorageError> {
        count_neighbor_select();

        let namespace = self.namespace.clone();
        let node_str = node_id.to_string();

        self.with_reader("neighbors", move |conn| {
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

            let (where_extra, limit_clause, extra_params) = neighbor_extra_clause(&query, 3);

            // Deterministic weight-descending order, tie-broken by node_id ascending,
            // applied BEFORE `LIMIT` — otherwise a `limit`/`fanout` cap can silently
            // drop high-weight neighbors in favor of arbitrary SQLite row order
            // (ADR-089 context-verb review).
            let full_sql = format!(
                "SELECT node_id, edge_id, relation, weight FROM ({}){} \
                 ORDER BY weight DESC, node_id ASC{}",
                sql, where_extra, limit_clause
            );

            let mut stmt = conn.prepare(&full_sql)?;

            let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            all_params.push(Box::new(namespace.clone()));
            all_params.push(Box::new(node_str.clone()));
            all_params.extend(extra_params);

            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                all_params.iter().map(|p| p.as_ref()).collect();

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
        .await
    }

    /// Single-query both-direction neighbor fetch (ADR-089 context-verb
    /// optimization): projects a `'out'`/`'in'` literal from each `UNION ALL`
    /// arm so the caller gets direction labels without a second direction-
    /// scoped round trip. `query.direction` is ignored — always both.
    async fn neighbors_both_directions(
        &self,
        node_id: Uuid,
        query: NeighborQuery,
    ) -> Result<Vec<DirectedNeighborHit>, StorageError> {
        count_neighbor_select();

        let namespace = self.namespace.clone();
        let node_str = node_id.to_string();

        self.with_reader("neighbors_both_directions", move |conn| {
            let base_out = "SELECT target_id AS node_id, id AS edge_id, relation, weight, \
                            'out' AS dir \
                            FROM graph_edges \
                            WHERE namespace = ?1 AND source_id = ?2 AND deleted_at IS NULL";
            let base_in = "SELECT source_id AS node_id, id AS edge_id, relation, weight, \
                           'in' AS dir \
                           FROM graph_edges \
                           WHERE namespace = ?1 AND target_id = ?2 AND deleted_at IS NULL";
            let sql = format!("{} UNION ALL {}", base_out, base_in);

            let (where_extra, limit_clause, extra_params) = neighbor_extra_clause(&query, 3);

            // Same global weight-descending/node_id-ascending order as `neighbors`
            // (ADR-089 context-verb review),
            // applied across BOTH directions before `LIMIT` truncates. A
            // reciprocal pair (an Out edge and an In edge to/from the same
            // neighbor at the same weight) ties on `(weight, node_id)`, so the
            // order is extended with a direction rank (`out` before `in`) and
            // finally `edge_id` to make the pre-`LIMIT` order fully
            // deterministic).
            let full_sql = format!(
                "SELECT node_id, edge_id, relation, weight, dir FROM ({}){} \
                 ORDER BY weight DESC, node_id ASC, \
                 CASE dir WHEN 'out' THEN 0 ELSE 1 END ASC, edge_id ASC{}",
                sql, where_extra, limit_clause
            );

            let mut stmt = conn.prepare(&full_sql)?;

            let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            all_params.push(Box::new(namespace.clone()));
            all_params.push(Box::new(node_str.clone()));
            all_params.extend(extra_params);

            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                all_params.iter().map(|p| p.as_ref()).collect();

            let rows = stmt.query_map(param_refs.as_slice(), |row| {
                let nid_str: String = row.get(0)?;
                let eid_str: String = row.get(1)?;
                let relation_str: String = row.get(2)?;
                let weight: f64 = row.get(3)?;
                let dir_str: String = row.get(4)?;
                Ok((nid_str, eid_str, relation_str, weight, dir_str))
            })?;

            let mut hits = Vec::new();
            for row in rows {
                let (nid_str, eid_str, relation_str, weight, dir_str) = row?;
                let relation = relation_str.parse::<EdgeRelation>().map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
                let direction = if dir_str == "out" {
                    Direction::Out
                } else {
                    Direction::In
                };
                hits.push(DirectedNeighborHit {
                    hit: NeighborHit {
                        node_id: parse_uuid(&nid_str)?,
                        edge_id: parse_uuid(&eid_str)?,
                        relation,
                        weight,
                        name: None,
                        kind: None,
                        entity_type: None,
                    },
                    direction,
                });
            }

            Ok(hits)
        })
        .await
    }

    async fn traverse(&self, request: TraversalRequest) -> Result<Vec<GraphPath>, StorageError> {
        use std::collections::{HashMap, HashSet};

        use khive_storage::types::Direction;

        if request.roots.is_empty() {
            return Ok(Vec::new());
        }

        let roots = request.roots.clone();
        let opts = request.options.clone();
        let include_roots = request.include_roots;
        let namespace = self.namespace.clone();
        let max_depth_i64 =
            i64::try_from(opts.max_depth).map_err(|_| StorageError::InvalidInput {
                capability: StorageCapability::Graph,
                operation: "traverse".into(),
                message: format!(
                    "TraversalOptions: max_depth must be <= i64::MAX, got {}",
                    opts.max_depth
                ),
            })?;

        self.with_reader("traverse", move |conn| {
            // Two SQLite limits apply to the seed VALUES clause:
            //
            //   1. SQLITE_LIMIT_COMPOUND_SELECT (default 500): SQLite counts each row in a
            //      VALUES list as one term in a compound SELECT.  Exceeding it gives
            //      "too many terms in compound SELECT".
            //
            //   2. SQLITE_LIMIT_VARIABLE_NUMBER (default 999): each root binds one parameter
            //      (referenced 3× in its seed row but counted once).  Fixed overhead —
            //      namespace, depth, optional relation/weight params — adds ~20 at most.
            //
            // 400 rows stays safely below both: 400 < 500 (compound) and
            // 400 + fixed << 999 (variables).
            const CHUNK_ROOTS: usize = 400;

            // Determine join direction (invariant across chunks).
            let (join_condition, next_node) = match opts.direction {
                Direction::Out => ("e.source_id = t.node_id", "e.target_id"),
                Direction::In => ("e.target_id = t.node_id", "e.source_id"),
                Direction::Both => (
                    "(e.source_id = t.node_id OR e.target_id = t.node_id)",
                    "CASE WHEN e.source_id = t.node_id THEN e.target_id ELSE e.source_id END",
                ),
            };

            // Open a deferred read transaction so ALL chunk queries observe the same
            // graph snapshot.  Without this, a writer committing between chunks could
            // let roots 1..400 see the pre-commit graph and 401..800 see the post-commit
            // graph.  One pool checkout, one snapshot for the full traverse.
            //
            // ADR-091 Plank 0: this is the most WAL-pin-relevant span in the store —
            // it intentionally holds a read snapshot across chunked traversal work.
            // Registered before the transaction is opened so the handle (declared
            // first) drops after `tx`'s own Drop runs (locals drop in reverse
            // declaration order within the same scope).
            let _tx_handle =
                khive_storage::tx_registry::register(Some("graph_traverse_read".to_string()));
            let tx = conn.unchecked_transaction()?;

            // Accumulate per-root state across all chunks: (nodes_with_path_weight, seen_set).
            // Each entry carries the PathNode and its cumulative path weight from the SQL row,
            // so the Rust-level per-root limit truncation can compute an accurate max_weight
            // over the kept nodes.
            let mut root_data: HashMap<Uuid, (Vec<(PathNode, f64)>, HashSet<Uuid>)> =
                HashMap::with_capacity(roots.len());

            // Pre-seed with root nodes when include_roots is set (done once for all roots).
            for root_id in &roots {
                let (nodes, seen) = root_data.entry(*root_id).or_default();
                if include_roots {
                    seen.insert(*root_id);
                    nodes.push((
                        PathNode {
                            node_id: *root_id,
                            via_edge: None,
                            depth: 0,
                            name: None,
                            kind: None,
                            properties: None,
                        },
                        0.0,
                    ));
                }
            }

            for chunk in roots.chunks(CHUNK_ROOTS) {
                let n_chunk = chunk.len();

                // Param layout (per-chunk, not total):
                //   ?1 .. ?{n_chunk}     — root UUID strings (each used 3× in seed row)
                //   ?{n_chunk + 1}       — namespace
                //   ?{n_chunk + 2}       — max_depth
                //   ?{n_chunk + 3} ..    — optional relation / weight params
                let ns_param = n_chunk + 1;
                let depth_param = n_chunk + 2;
                let mut extra_param_idx = n_chunk + 3;

                let mut relation_cond = String::new();
                let mut extra_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

                if let Some(ref rels) = opts.relations {
                    if !rels.is_empty() {
                        let placeholders: Vec<String> = rels
                            .iter()
                            .map(|r| {
                                extra_params.push(Box::new(r.to_string()));
                                let p = format!("?{extra_param_idx}");
                                extra_param_idx += 1;
                                p
                            })
                            .collect();
                        relation_cond = format!(" AND e.relation IN ({})", placeholders.join(","));
                    }
                }

                let mut weight_cond = String::new();
                if let Some(min_w) = opts.min_weight {
                    extra_params.push(Box::new(min_w));
                    weight_cond = format!(" AND e.weight >= ?{extra_param_idx}");
                    // limit is applied in Rust (see below), so no SQL param needed.
                }

                // Seed rows: one per root in this chunk, each referencing its own
                // param 3× (root_id, node_id, and the initial path — a JSON array
                // containing just the root id).
                let seed_rows: Vec<String> = (1..=n_chunk)
                    .map(|i| format!("(?{i}, ?{i}, NULL, 0, json_array(?{i}), 0.0)"))
                    .collect();
                let seeds = seed_rows.join(", ");

                // CTE covering the chunk's roots.  CROSS JOIN forces SQLite to put
                // the frontier (t) as the outer loop and seek graph_edges by index,
                // avoiding the O(edges × frontier) plan (#250, #251).
                //
                // `path` is a JSON array of visited node-id strings rather than a
                // comma-joined string: cycle detection needs exact set membership,
                // and a LIKE-based substring test over a delimited string false-
                // matches whenever one node id is a substring of another (#562).
                // `json_each` gives an exact per-element equality check instead.
                let cte_sql = format!(
                    "WITH RECURSIVE traversal(\
                         root_id, node_id, edge_id, depth, path, total_weight\
                     ) AS (\
                         VALUES {seeds} \
                         UNION ALL \
                         SELECT t.root_id, {next_node}, e.id, t.depth + 1, \
                                json_insert(t.path, '$[#]', {next_node}), \
                                t.total_weight + e.weight \
                         FROM traversal t CROSS JOIN graph_edges e \
                             ON {join_condition} \
                         WHERE e.namespace = ?{ns} \
                           AND e.deleted_at IS NULL \
                           AND t.depth < ?{depth} \
                           AND NOT EXISTS (\
                               SELECT 1 FROM json_each(t.path) WHERE value = {next_node}\
                           )\
                           {rel_cond}{wt_cond} \
                     ) \
                     SELECT root_id, node_id, edge_id, depth, total_weight \
                     FROM traversal WHERE depth > 0 \
                     ORDER BY root_id, depth",
                    seeds = seeds,
                    next_node = next_node,
                    join_condition = join_condition,
                    ns = ns_param,
                    depth = depth_param,
                    rel_cond = relation_cond,
                    wt_cond = weight_cond,
                );

                let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                for root_id in chunk {
                    all_params.push(Box::new(root_id.to_string()));
                }
                all_params.push(Box::new(namespace.clone()));
                all_params.push(Box::new(max_depth_i64));
                all_params.extend(extra_params);

                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    all_params.iter().map(|p| p.as_ref()).collect();

                // Queries run on `conn`; reads are connection-level and participate
                // in the open `tx` deferred snapshot.
                let mut stmt = conn.prepare(&cte_sql)?;
                let rows_iter = stmt.query_map(param_refs.as_slice(), |row| {
                    let root_str: String = row.get(0)?;
                    let node_str: String = row.get(1)?;
                    let edge_str: Option<String> = row.get(2)?;
                    let depth: i64 = row.get(3)?;
                    let total_weight: f64 = row.get(4)?;
                    Ok((root_str, node_str, edge_str, depth, total_weight))
                })?;

                // The CTE is ordered by (root_id, depth), so the first occurrence of
                // each (root_id, node_id) pair is the shallowest — that is the one we
                // keep (BFS first-visit semantics, matching #285).
                for row in rows_iter {
                    let (root_str, node_str, edge_str, depth, total_weight) = row?;
                    let root_id = parse_uuid(&root_str)?;
                    let node_id = parse_uuid(&node_str)?;
                    let (nodes, seen) = root_data.entry(root_id).or_default();
                    if !seen.insert(node_id) {
                        continue;
                    }
                    let via_edge = edge_str.map(|s| parse_uuid(&s)).transpose()?;
                    nodes.push((
                        PathNode {
                            node_id,
                            via_edge,
                            depth: depth as usize,
                            name: None,
                            kind: None,
                            properties: None,
                        },
                        total_weight,
                    ));
                }
            }

            tx.commit()?;

            // Reconstruct Vec<GraphPath> in original root order.
            // Per-root limit: counts only non-root nodes against the cap, matching
            // the original per-root-CTE semantics where the SQL LIMIT applied only
            // to depth > 0 rows.  Truncation is on the post-dedup list (BFS order),
            // so the shallowest `limit` reachable nodes are kept per root.
            let mut all_paths: Vec<GraphPath> = Vec::with_capacity(roots.len());
            for root_id in &roots {
                if let Some((mut nw, _)) = root_data.remove(root_id) {
                    if nw.is_empty() {
                        continue;
                    }
                    if let Some(lim) = opts.limit {
                        let root_count = usize::from(include_roots);
                        nw.truncate(root_count + lim as usize);
                    }
                    // Post-truncation guard: a limit=0 + include_roots=false call
                    // truncates to zero nodes; there is nothing to emit.
                    if nw.is_empty() {
                        continue;
                    }
                    let max_weight = nw.iter().map(|(_, w)| *w).fold(0.0_f64, f64::max);
                    let nodes: Vec<PathNode> = nw.into_iter().map(|(n, _)| n).collect();
                    all_paths.push(GraphPath {
                        root_id: *root_id,
                        nodes,
                        total_weight: max_weight,
                    });
                }
            }

            Ok(all_paths)
        })
        .await
    }

    async fn purge_incident_edges(&self, node_id: Uuid) -> Result<u64, StorageError> {
        // No namespace filter: UUID v4 is globally unique. Hard-delete cascade must
        // remove ALL incident edges regardless of which namespace they were written in
        // (ADR-002 no-dangling-references, ADR-007 by-ID contract).
        let statement = purge_incident_edges_statement(node_id);
        self.with_writer("purge_incident_edges", move |conn| {
            let mut stmt = conn.prepare(&statement.sql)?;
            bind_params(&mut stmt, &statement.params)?;
            Ok(stmt.raw_execute()? as u64)
        })
        .await
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
#[path = "graph_tests.rs"]
mod tests;
