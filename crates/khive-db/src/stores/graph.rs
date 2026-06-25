//! SQL-backed `GraphStore`: edge CRUD, neighbor queries, and recursive CTE traversal.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use uuid::Uuid;

use khive_storage::error::StorageError;
use khive_storage::types::{
    BatchWriteSummary, DeleteMode, Edge, EdgeFilter, EdgeSortField, GraphPath, NeighborHit,
    NeighborQuery, Page, PageRequest, PathNode, SortDirection, SortOrder, TraversalRequest,
};
use khive_storage::GraphStore;
use khive_storage::LinkId;
use khive_storage::StorageCapability;
use khive_types::EdgeRelation;

use crate::error::SqliteError;
use crate::pool::ConnectionPool;

/// Map a rusqlite error to `StorageError` with `Graph` capability.
fn map_err(e: rusqlite::Error, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Graph, op, e)
}

fn map_sqlite_err(e: SqliteError, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Graph, op, e)
}

/// A GraphStore backed by SQLite tables.
pub struct SqlGraphStore {
    pool: Arc<ConnectionPool>,
    is_file_backed: bool,
    /// Default namespace for multi-record queries (ADR-007 PARAM-ONLY: used as a
    /// WHERE filter on `query_edges`/`neighbors`/`traverse`, never as an
    /// enforcement gate on by-ID operations).
    namespace: String,
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
        Self {
            pool,
            is_file_backed,
            namespace: namespace.into(),
        }
    }

    fn open_standalone_writer(&self) -> Result<rusqlite::Connection, StorageError> {
        let config = self.pool.config();
        let path = config.path.as_ref().ok_or_else(|| StorageError::Pool {
            operation: "graph_writer".into(),
            message: "in-memory databases do not support standalone connections".into(),
        })?;

        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| map_err(e, "open_graph_writer"))?;

        conn.busy_timeout(config.busy_timeout)
            .map_err(|e| map_err(e, "open_graph_writer"))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| map_err(e, "open_graph_writer"))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| map_err(e, "open_graph_writer"))?;

        Ok(conn)
    }

    fn open_standalone_reader(&self) -> Result<rusqlite::Connection, StorageError> {
        let config = self.pool.config();
        let path = config.path.as_ref().ok_or_else(|| StorageError::Pool {
            operation: "graph_reader".into(),
            message: "in-memory databases do not support standalone connections".into(),
        })?;

        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| map_err(e, "open_graph_reader"))?;

        conn.busy_timeout(config.busy_timeout)
            .map_err(|e| map_err(e, "open_graph_reader"))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| map_err(e, "open_graph_reader"))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| map_err(e, "open_graph_reader"))?;

        Ok(conn)
    }

    async fn with_writer<F, R>(&self, op: &'static str, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
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

#[async_trait]
impl GraphStore for SqlGraphStore {
    async fn upsert_edge(&self, edge: Edge) -> Result<(), StorageError> {
        let namespace = edge.namespace.clone();
        let id_str = Uuid::from(edge.id).to_string();
        let (source_id, target_id) =
            canonical_edge_endpoints(edge.relation, edge.source_id, edge.target_id);
        let src_str = source_id.to_string();
        let tgt_str = target_id.to_string();
        let relation_str = edge.relation.to_string();
        let metadata_str = edge
            .metadata
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| StorageError::driver(StorageCapability::Graph, "upsert_edge", e))?;
        self.with_writer("upsert_edge", move |conn| {
            conn.execute(
                "INSERT INTO graph_edges \
                 (namespace, id, source_id, target_id, relation, weight, \
                  created_at, updated_at, deleted_at, metadata, target_backend) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
                 ON CONFLICT(namespace, id) DO UPDATE SET \
                     source_id = excluded.source_id, \
                     target_id = excluded.target_id, \
                     relation = excluded.relation, \
                     weight = excluded.weight, \
                     updated_at = excluded.updated_at, \
                     deleted_at = NULL, \
                     metadata = excluded.metadata, \
                     target_backend = excluded.target_backend \
                 ON CONFLICT(namespace, source_id, target_id, relation) DO UPDATE SET \
                     weight = excluded.weight, \
                     updated_at = excluded.updated_at, \
                     deleted_at = NULL, \
                     metadata = excluded.metadata, \
                     target_backend = excluded.target_backend",
                rusqlite::params![
                    namespace,
                    id_str,
                    src_str,
                    tgt_str,
                    relation_str,
                    edge.weight,
                    edge.created_at.timestamp_micros(),
                    edge.updated_at.timestamp_micros(),
                    edge.deleted_at.map(|t| t.timestamp_micros()),
                    metadata_str,
                    edge.target_backend,
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn upsert_edges(&self, edges: Vec<Edge>) -> Result<BatchWriteSummary, StorageError> {
        let attempted = edges.len() as u64;

        self.with_writer("upsert_edges", move |conn| {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let mut affected = 0u64;

            for edge in &edges {
                let id_str = Uuid::from(edge.id).to_string();
                let (canon_src, canon_tgt) =
                    canonical_edge_endpoints(edge.relation, edge.source_id, edge.target_id);
                let src_str = canon_src.to_string();
                let tgt_str = canon_tgt.to_string();
                let relation_str = edge.relation.to_string();
                let metadata_str = edge
                    .metadata
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                if let Err(e) = conn.execute(
                    "INSERT INTO graph_edges \
                     (namespace, id, source_id, target_id, relation, weight, \
                      created_at, updated_at, deleted_at, metadata, target_backend) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
                     ON CONFLICT(namespace, id) DO UPDATE SET \
                         source_id = excluded.source_id, \
                         target_id = excluded.target_id, \
                         relation = excluded.relation, \
                         weight = excluded.weight, \
                         updated_at = excluded.updated_at, \
                         deleted_at = NULL, \
                         metadata = excluded.metadata, \
                         target_backend = excluded.target_backend \
                     ON CONFLICT(namespace, source_id, target_id, relation) DO UPDATE SET \
                         weight = excluded.weight, \
                         updated_at = excluded.updated_at, \
                         deleted_at = NULL, \
                         metadata = excluded.metadata, \
                         target_backend = excluded.target_backend",
                    rusqlite::params![
                        edge.namespace.as_str(),
                        id_str,
                        src_str,
                        tgt_str,
                        relation_str,
                        edge.weight,
                        edge.created_at.timestamp_micros(),
                        edge.updated_at.timestamp_micros(),
                        edge.deleted_at.map(|t| t.timestamp_micros()),
                        metadata_str,
                        edge.target_backend.as_deref(),
                    ],
                ) {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(e);
                }
                affected += 1;
            }

            if let Err(e) = conn.execute_batch("COMMIT") {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
            Ok(BatchWriteSummary {
                attempted,
                affected,
                failed: 0,
                first_error: String::new(),
            })
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
        // Chunk source IDs to stay within SQLite variable limit.
        // ?1 = namespace, ?2..?N+1 = sources, then optional filter params.
        const CHUNK: usize = 880;
        let namespace = self.namespace.clone();
        let mut result: Vec<(Uuid, NeighborHit)> = Vec::new();

        for chunk in sources.chunks(CHUNK) {
            let chunk_owned: Vec<Uuid> = chunk.to_vec();
            let query_clone = query.clone();
            let ns = namespace.clone();

            let pairs = self
                .with_reader("batch_neighbors", move |conn| {
                    let src_strs: Vec<String> = chunk_owned.iter().map(|u| u.to_string()).collect();

                    // Build the inner SELECT for one direction, using positional
                    // params starting at `first_src_param` for the source IN-list.
                    // Returns (sql_fragment, extra_param_values) where extra_param_values
                    // covers relations and min_weight filters only (NOT the limit).
                    let build_inner_sql =
                        |direction_out: bool,
                         first_src_param: usize,
                         q: &NeighborQuery|
                         -> (String, Vec<String>, Option<f64>) {
                            let placeholders: Vec<String> = (first_src_param
                                ..first_src_param + src_strs.len())
                                .map(|i| format!("?{i}"))
                                .collect();
                            let in_list = placeholders.join(",");

                            let (origin_col, filter_col, node_col) = if direction_out {
                                ("source_id", "source_id", "target_id")
                            } else {
                                ("target_id", "target_id", "source_id")
                            };

                            let mut rel_params: Vec<String> = Vec::new();
                            let mut conditions: Vec<String> = Vec::new();
                            let mut param_idx = first_src_param + src_strs.len();

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
                                    conditions.push(format!("relation IN ({})", ps.join(",")));
                                }
                            }

                            // min_weight is returned separately so it can be added to
                            // all_params AFTER the rel_params block, at the right index.
                            let min_weight_val = if let Some(min_w) = q.min_weight {
                                conditions.push(format!("weight >= ?{param_idx}"));
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
                                "SELECT {origin_col} AS origin_id, {node_col} AS node_id, \
                             id AS edge_id, relation, weight \
                             FROM graph_edges \
                             WHERE namespace = ?1 AND {filter_col} IN ({in_list}) \
                               AND deleted_at IS NULL{where_extra}",
                            );
                            (sql, rel_params, min_weight_val)
                        };

                    // For Direction::Both we need to build a UNION ALL of both inner
                    // selects and then apply the per-source ROW_NUMBER limit ONCE over
                    // the combined set.  This matches the single-source neighbors()
                    // behaviour where Both uses a single UNION ALL + one outer LIMIT.
                    //
                    // Param layout:
                    //   Out/In:  ?1=ns  ?2..?N+1=srcs  ?extras...  [?limit]
                    //   Both:    ?1=ns  ?2..?N+1=out_srcs  out_extras...
                    //                   ?M..?M+N=in_srcs   in_extras...  [?limit]
                    //
                    // `build_inner_sql` receives `first_src_param` so it generates the
                    // correct placeholder indices for each half.

                    let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                    all_params.push(Box::new(ns.to_string())); // ?1

                    let combined_inner: String;
                    let limit_param_idx: usize;

                    match query_clone.direction {
                        Direction::Out | Direction::In => {
                            let direction_out = matches!(query_clone.direction, Direction::Out);
                            let (sql, rel_params, min_weight_val) =
                                build_inner_sql(direction_out, 2, &query_clone);
                            combined_inner = sql;

                            // Bind: ?1=ns (done), ?2..?N+1=srcs, rel_params, [min_weight]
                            for s in &src_strs {
                                all_params.push(Box::new(s.clone()));
                            }
                            for r in rel_params {
                                all_params.push(Box::new(r));
                            }
                            if let Some(mw) = min_weight_val {
                                all_params.push(Box::new(mw));
                            }
                            limit_param_idx = all_params.len() + 1;
                        }
                        Direction::Both => {
                            // Out half: src params at ?2..?N+1
                            let (out_sql, out_rels, out_mw) =
                                build_inner_sql(true, 2, &query_clone);
                            let after_out_srcs = 2 + src_strs.len();
                            let after_out_rels = after_out_srcs + out_rels.len();
                            let after_out_mw =
                                after_out_rels + if out_mw.is_some() { 1 } else { 0 };
                            let in_first = after_out_mw;

                            // In half: src params start at `in_first`
                            let (in_sql, in_rels, in_mw) =
                                build_inner_sql(false, in_first, &query_clone);

                            combined_inner = format!("{out_sql} UNION ALL {in_sql}");

                            // Bind layout: ns | out_srcs | out_rels | [out_mw] | in_srcs | in_rels | [in_mw]
                            for s in &src_strs {
                                all_params.push(Box::new(s.clone())); // out sources
                            }
                            for r in out_rels {
                                all_params.push(Box::new(r));
                            }
                            if let Some(mw) = out_mw {
                                all_params.push(Box::new(mw));
                            }
                            for s in &src_strs {
                                all_params.push(Box::new(s.clone())); // in sources
                            }
                            for r in in_rels {
                                all_params.push(Box::new(r));
                            }
                            if let Some(mw) = in_mw {
                                all_params.push(Box::new(mw));
                            }
                            limit_param_idx = all_params.len() + 1;
                        }
                    }

                    // Wrap combined inner with per-source ROW_NUMBER limit if needed.
                    let full_sql = if let Some(lim) = query_clone.limit {
                        all_params.push(Box::new(lim as i64));
                        format!(
                            "SELECT origin_id, node_id, edge_id, relation, weight \
                             FROM (SELECT *, ROW_NUMBER() OVER (PARTITION BY origin_id) AS rn \
                                   FROM ({combined_inner})) WHERE rn <= ?{limit_param_idx}",
                        )
                    } else {
                        format!(
                            "SELECT origin_id, node_id, edge_id, relation, weight \
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
                            },
                        ));
                    }
                    Ok(pairs)
                })
                .await?;
            result.extend(pairs);
        }
        Ok(result)
    }

    async fn delete_edge(&self, id: LinkId, mode: DeleteMode) -> Result<bool, StorageError> {
        let id_str = Uuid::from(id).to_string();

        self.with_writer("delete_edge", move |conn| {
            let affected = match mode {
                DeleteMode::Soft => conn.execute(
                    "UPDATE graph_edges SET deleted_at = ?2, updated_at = ?2 \
                     WHERE id = ?1 AND deleted_at IS NULL",
                    rusqlite::params![id_str, chrono::Utc::now().timestamp_micros()],
                )?,
                DeleteMode::Hard => conn.execute(
                    "DELETE FROM graph_edges WHERE id = ?1",
                    rusqlite::params![id_str],
                )?,
            };
            Ok(affected > 0)
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
            all_params.push(Box::new(page.limit as i64));
            all_params.push(Box::new(page.offset as i64));

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

    async fn neighbors(
        &self,
        node_id: Uuid,
        query: NeighborQuery,
    ) -> Result<Vec<NeighborHit>, StorageError> {
        use khive_storage::types::Direction;

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

            let mut conditions: Vec<String> = Vec::new();
            let mut extra_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            let mut param_idx = 3;

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

            let full_sql = format!(
                "SELECT node_id, edge_id, relation, weight FROM ({}){}{}",
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
                });
            }

            Ok(hits)
        })
        .await
    }

    async fn traverse(&self, request: TraversalRequest) -> Result<Vec<GraphPath>, StorageError> {
        use khive_storage::types::Direction;

        if request.roots.is_empty() {
            return Ok(Vec::new());
        }

        let roots = request.roots.clone();
        let opts = request.options.clone();
        let include_roots = request.include_roots;
        let namespace = self.namespace.clone();

        self.with_reader("traverse", move |conn| {
            let mut all_paths: Vec<GraphPath> = Vec::new();

            for root_id in &roots {
                let root_str = root_id.to_string();

                let (join_condition, next_node) = match opts.direction {
                    Direction::Out => ("e.source_id = t.node_id", "e.target_id"),
                    Direction::In => ("e.target_id = t.node_id", "e.source_id"),
                    Direction::Both => (
                        "(e.source_id = t.node_id OR e.target_id = t.node_id)",
                        "CASE WHEN e.source_id = t.node_id THEN e.target_id ELSE e.source_id END",
                    ),
                };

                let mut relation_cond = String::new();
                let mut relation_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                let mut param_idx = 4;

                if let Some(ref rels) = opts.relations {
                    if !rels.is_empty() {
                        let placeholders: Vec<String> = rels
                            .iter()
                            .map(|r| {
                                relation_params.push(Box::new(r.to_string()));
                                let p = format!("?{}", param_idx);
                                param_idx += 1;
                                p
                            })
                            .collect();
                        relation_cond =
                            format!(" AND e.relation IN ({})", placeholders.join(","));
                    }
                }

                let mut weight_cond = String::new();
                if let Some(min_w) = opts.min_weight {
                    relation_params.push(Box::new(min_w));
                    weight_cond = format!(" AND e.weight >= ?{}", param_idx);
                    param_idx += 1;
                }

                let limit_clause = if let Some(lim) = opts.limit {
                    relation_params.push(Box::new(lim as i64));
                    format!(" LIMIT ?{}", param_idx)
                } else {
                    String::new()
                };

                let cte_sql = format!(
                    "WITH RECURSIVE traversal(node_id, edge_id, depth, path, total_weight) AS (\
                         SELECT ?2, NULL, 0, ?2, 0.0 \
                         UNION ALL \
                         SELECT {next_node}, e.id, t.depth + 1, \
                                t.path || ',' || {next_node}, \
                                t.total_weight + e.weight \
                         FROM graph_edges e \
                         JOIN traversal t ON {join_condition} \
                         WHERE e.namespace = ?1 \
                           AND e.deleted_at IS NULL \
                           AND t.depth < ?3 \
                           AND (',' || t.path || ',') NOT LIKE '%,' || {next_node} || ',%'{rel_cond}{wt_cond} \
                     ) \
                     SELECT node_id, edge_id, depth, path, total_weight \
                     FROM traversal WHERE depth > 0 \
                     ORDER BY depth{limit}",
                    next_node = next_node,
                    join_condition = join_condition,
                    rel_cond = relation_cond,
                    wt_cond = weight_cond,
                    limit = limit_clause,
                );

                let mut stmt = conn.prepare(&cte_sql)?;

                let mut all_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                all_params.push(Box::new(namespace.clone()));
                all_params.push(Box::new(root_str.clone()));
                all_params.push(Box::new(opts.max_depth as i64));
                all_params.extend(relation_params);

                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    all_params.iter().map(|p| p.as_ref()).collect();

                let rows = stmt.query_map(param_refs.as_slice(), |row| {
                    let node_str: String = row.get(0)?;
                    let edge_str: Option<String> = row.get(1)?;
                    let depth: i64 = row.get(2)?;
                    let _path: String = row.get(3)?;
                    let total_weight: f64 = row.get(4)?;
                    Ok((node_str, edge_str, depth, total_weight))
                })?;

                let mut nodes = Vec::new();
                let mut max_weight = 0.0f64;
                // Track visited node IDs to deduplicate multi-path reachability
                // (#285). Rows are ordered by depth (shallowest first), so the
                // first occurrence is the BFS-order first-visit — that is the
                // one we keep.
                let mut seen: std::collections::HashSet<Uuid> =
                    std::collections::HashSet::new();

                if include_roots {
                    seen.insert(*root_id);
                    nodes.push(PathNode {
                        node_id: *root_id,
                        via_edge: None,
                        depth: 0,
                        name: None,
                        kind: None,
                    });
                }

                for row in rows {
                    let (node_str, edge_str, depth, total_weight) = row?;
                    let node_id = parse_uuid(&node_str)?;
                    // Skip nodes already seen via an earlier (shallower) path.
                    if !seen.insert(node_id) {
                        continue;
                    }
                    let via_edge = edge_str.map(|s| parse_uuid(&s)).transpose()?;
                    nodes.push(PathNode {
                        node_id,
                        via_edge,
                        depth: depth as usize,
                        name: None,
                        kind: None,
                    });
                    if total_weight > max_weight {
                        max_weight = total_weight;
                    }
                }

                if nodes.len() > if include_roots { 1 } else { 0 } || include_roots {
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
        let id_str = node_id.to_string();
        // No namespace filter: UUID v4 is globally unique. Hard-delete cascade must
        // remove ALL incident edges regardless of which namespace they were written in
        // (ADR-002 no-dangling-references, ADR-007 by-ID contract).
        self.with_writer("purge_incident_edges", move |conn| {
            let affected = conn.execute(
                "DELETE FROM graph_edges WHERE source_id = ?1 OR target_id = ?1",
                rusqlite::params![id_str],
            )?;
            Ok(affected as u64)
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
