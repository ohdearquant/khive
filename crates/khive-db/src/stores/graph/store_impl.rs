use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use khive_storage::graph::{SymmetricEdgeUpdateOutcome, SymmetricEdgeUpdateRequest};
use khive_storage::types::{
    BatchWriteSummary, DeleteMode, DirectedNeighborHit, Direction, Edge, EdgeEndpointBaseCounts,
    EdgeFilter, EdgeSeekPage, EdgeSortField, EdgeUpsertRefusal, EdgeUpsertRequest,
    EdgeUpsertResult, GraphPath, GuardedBatchOutcome, GuardedBatchRefusal,
    GuardedEdgeBatchUpsertOutcome, GuardedEdgeUpsertOutcome, GuardedWriteOutcome, NeighborCursor,
    NeighborHit, NeighborQuery, Page, PageRequest, SeekCursor, SeekPage, SortOrder,
    TraversalRequest,
};
use khive_storage::{GraphStore, LinkId, StorageCapability, StorageError, StorageResult};
use khive_types::EdgeRelation;
use rusqlite::OptionalExtension;
use uuid::Uuid;

use crate::sql_bridge::bind_params;

use super::edge_reads::{
    build_edge_filter_sql, build_edge_filter_sql_for_namespaces,
    build_edge_filter_sql_for_namespaces_json, count_neighbor_select, edge_order_clause,
    endpoint_base_case, fold_endpoint_base_row, neighbor_extra_clause, parse_uuid, read_edge,
    with_live_endpoints, LIVE_ENDPOINTS_CONDITION,
};
use super::edge_statements::{
    canonical_edge_endpoints, edge_hard_delete_statement, edge_insert_if_absent_statement,
    edge_replace_if_unchanged_statement, edge_soft_delete_statement,
    purge_incident_edges_statement,
};
use super::traversal::{run_bounded_traversal, traversal_neighbor_sql, traversal_timeout_error};
use super::{
    map_err, report_graph_usage, resurrection_required_error, SqlGraphStore,
    LATEST_ANNOTATING_NOTE_SQL, LATEST_ANNOTATING_NOTE_WITH_PROPERTY_SQL,
    NAMESPACE_COUNT_CHUNK_SIZE,
};

#[async_trait]
impl GraphStore for SqlGraphStore {
    async fn update_symmetric_edge_if_unchanged(
        &self,
        request: SymmetricEdgeUpdateRequest,
    ) -> StorageResult<SymmetricEdgeUpdateOutcome> {
        self.update_symmetric_edge(request).await
    }

    async fn latest_annotating_note(
        &self,
        node_id: Uuid,
        kind: &str,
        tag: &str,
    ) -> Result<Option<(Uuid, i64)>, StorageError> {
        let namespace = self.namespace.clone();
        let node_id = node_id.to_string();
        let kind = kind.to_owned();
        let tag = tag.to_owned();
        self.with_indexed_reader("latest_annotating_note", move |conn| {
            conn.query_row(
                LATEST_ANNOTATING_NOTE_SQL,
                rusqlite::params![namespace, node_id, kind, tag],
                |row| {
                    let id: String = row.get(0)?;
                    Ok((parse_uuid(&id)?, row.get(1)?))
                },
            )
            .optional()
        })
        .await
    }

    async fn latest_annotating_note_with_property(
        &self,
        node_id: Uuid,
        kind: &str,
        tag: &str,
        property_key: &str,
        property_value: &str,
    ) -> Result<Option<(Uuid, i64)>, StorageError> {
        let namespace = self.namespace.clone();
        let node_id = node_id.to_string();
        let kind = kind.to_owned();
        let tag = tag.to_owned();
        let property_key = property_key.to_owned();
        let property_value = property_value.to_owned();
        self.with_indexed_reader("latest_annotating_note_with_property", move |conn| {
            conn.query_row(
                LATEST_ANNOTATING_NOTE_WITH_PROPERTY_SQL,
                rusqlite::params![namespace, node_id, kind, tag, property_key, property_value],
                |row| {
                    let id: String = row.get(0)?;
                    Ok((parse_uuid(&id)?, row.get(1)?))
                },
            )
            .optional()
        })
        .await
    }

    async fn upsert_edge(&self, edge: Edge) -> Result<(), StorageError> {
        self.upsert_edge_observed(EdgeUpsertRequest {
            edge,
            resurrect: false,
        })
        .await
        .map(|_| ())
    }

    async fn upsert_edge_observed(
        &self,
        request: EdgeUpsertRequest,
    ) -> Result<EdgeUpsertResult, StorageError> {
        match self
            .observed_edge_write("upsert_edge_observed", request, false)
            .await?
        {
            GuardedEdgeUpsertOutcome::Written(result) => Ok(result),
            GuardedEdgeUpsertOutcome::Refused(EdgeUpsertRefusal::ResurrectionRequired { edge }) => {
                Err(resurrection_required_error("upsert_edge_observed", &edge))
            }
            GuardedEdgeUpsertOutcome::Refused(EdgeUpsertRefusal::MissingEndpoints(_)) => {
                Err(StorageError::Conflict {
                    capability: StorageCapability::Graph,
                    operation: "upsert_edge_observed".into(),
                    message: "unguarded edge upsert reported a missing-endpoint refusal".into(),
                })
            }
        }
    }

    async fn insert_edge_if_absent(&self, edge: Edge) -> Result<bool, StorageError> {
        let statement = edge_insert_if_absent_statement(&edge);
        self.with_writer("insert_edge_if_absent", move |conn| {
            let mut stmt = conn.prepare_cached(&statement.sql)?;
            bind_params(&mut stmt, &statement.params)?;
            Ok(stmt.raw_execute()? > 0)
        })
        .await
    }

    async fn replace_edge_if_unchanged(
        &self,
        edge: Edge,
        expected_updated_at: DateTime<Utc>,
        expected_deleted_at: Option<DateTime<Utc>>,
    ) -> Result<bool, StorageError> {
        let statement =
            edge_replace_if_unchanged_statement(&edge, expected_updated_at, expected_deleted_at);
        self.with_writer("replace_edge_if_unchanged", move |conn| {
            let mut stmt = conn.prepare_cached(&statement.sql)?;
            bind_params(&mut stmt, &statement.params)?;
            Ok(stmt.raw_execute()? > 0)
        })
        .await
    }

    async fn upsert_edges(&self, edges: Vec<Edge>) -> Result<BatchWriteSummary, StorageError> {
        let attempted = edges.len() as u64;
        let requests = edges
            .into_iter()
            .map(|edge| EdgeUpsertRequest {
                edge,
                resurrect: false,
            })
            .collect();
        let outcome = self
            .observed_edge_batch_write("upsert_edges", requests, false)
            .await?;
        if let Some(refusal) = outcome.refusal {
            return match refusal.reason {
                EdgeUpsertRefusal::ResurrectionRequired { edge } => {
                    Err(resurrection_required_error("upsert_edges", &edge))
                }
                EdgeUpsertRefusal::MissingEndpoints(_) => Err(StorageError::Conflict {
                    capability: StorageCapability::Graph,
                    operation: "upsert_edges".into(),
                    message: "unguarded edge batch reported a missing-endpoint refusal".into(),
                }),
            };
        }
        Ok(BatchWriteSummary {
            attempted,
            affected: outcome.rows.len() as u64,
            ..BatchWriteSummary::default()
        })
    }

    async fn upsert_edge_guarded(&self, edge: Edge) -> Result<GuardedWriteOutcome, StorageError> {
        match self
            .upsert_edge_guarded_observed(EdgeUpsertRequest {
                edge,
                resurrect: false,
            })
            .await?
        {
            GuardedEdgeUpsertOutcome::Written(_) => Ok(GuardedWriteOutcome::Written),
            GuardedEdgeUpsertOutcome::Refused(EdgeUpsertRefusal::MissingEndpoints(missing)) => {
                Ok(GuardedWriteOutcome::Refused(missing))
            }
            GuardedEdgeUpsertOutcome::Refused(EdgeUpsertRefusal::ResurrectionRequired { edge }) => {
                Err(resurrection_required_error("upsert_edge_guarded", &edge))
            }
        }
    }

    async fn upsert_edge_guarded_observed(
        &self,
        request: EdgeUpsertRequest,
    ) -> Result<GuardedEdgeUpsertOutcome, StorageError> {
        self.observed_edge_write("upsert_edge_guarded_observed", request, true)
            .await
    }

    async fn upsert_edges_guarded(
        &self,
        edges: Vec<Edge>,
    ) -> Result<GuardedBatchOutcome, StorageError> {
        let attempted = edges.len() as u64;
        let requests = edges
            .iter()
            .cloned()
            .map(|edge| EdgeUpsertRequest {
                edge,
                resurrect: false,
            })
            .collect();
        let outcome = self.upsert_edges_guarded_observed(requests).await?;
        match outcome.refusal {
            None => Ok(GuardedBatchOutcome {
                summary: BatchWriteSummary {
                    attempted,
                    affected: outcome.rows.len() as u64,
                    ..BatchWriteSummary::default()
                },
                refused: None,
            }),
            Some(refusal) => match refusal.reason {
                EdgeUpsertRefusal::MissingEndpoints(missing) => {
                    let index = refusal.entry_index;
                    let edge = &edges[index];
                    let (source_id, target_id) =
                        canonical_edge_endpoints(edge.relation, edge.source_id, edge.target_id);
                    let message = format!(
                        "batch entry {index}: edge endpoint no longer exists at write time: source \
                         {source_id} or target {target_id}"
                    );
                    let mut summary = BatchWriteSummary {
                        attempted,
                        ..BatchWriteSummary::default()
                    };
                    // Keep the culprit's legacy first_error while recording
                    // per-input error classes and retryability in input order.
                    summary.first_error = message.clone();
                    let refusal = GuardedBatchRefusal {
                        entry_index: index,
                        missing,
                    };
                    for (failed_index, failed_edge) in edges.iter().enumerate() {
                        refusal.record_failure(&mut summary, failed_index, failed_edge, &message);
                    }
                    Ok(GuardedBatchOutcome {
                        summary,
                        refused: Some(refusal),
                    })
                }
                EdgeUpsertRefusal::ResurrectionRequired { edge } => {
                    Err(resurrection_required_error("upsert_edges_guarded", &edge))
                }
            },
        }
    }

    async fn upsert_edges_guarded_observed(
        &self,
        requests: Vec<EdgeUpsertRequest>,
    ) -> Result<GuardedEdgeBatchUpsertOutcome, StorageError> {
        self.observed_edge_batch_write("upsert_edges_guarded_observed", requests, true)
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

    async fn edge_sequence(&self, id: Uuid) -> Result<Option<i64>, StorageError> {
        let id = id.to_string();
        self.with_reader("edge_sequence", move |conn| {
            conn.query_row(
                "SELECT seq FROM graph_edges_seq WHERE edge_id = ?1",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .optional()
        })
        .await
    }

    async fn edge_sequences(&self, ids: &[Uuid]) -> Result<Vec<(Uuid, i64)>, StorageError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids = ids.to_vec();
        self.with_reader("edge_sequences", move |conn| {
            const CHUNK: usize = 900;
            let mut resolved = Vec::with_capacity(ids.len());
            for chunk in ids.chunks(CHUNK) {
                let placeholders = (1..=chunk.len())
                    .map(|index| format!("?{index}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!(
                    "SELECT edge_id, seq FROM graph_edges_seq WHERE edge_id IN ({placeholders})"
                );
                let strings = chunk.iter().map(Uuid::to_string).collect::<Vec<_>>();
                let params = strings
                    .iter()
                    .map(|id| id as &dyn rusqlite::types::ToSql)
                    .collect::<Vec<_>>();
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(params.as_slice(), |row| {
                    let id: String = row.get(0)?;
                    Ok((parse_uuid(&id)?, row.get::<_, i64>(1)?))
                })?;
                resolved.extend(rows.collect::<Result<Vec<_>, _>>()?);
            }
            Ok(resolved)
        })
        .await
    }

    async fn get_edge_by_natural_key_including_deleted(
        &self,
        namespace: &str,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
    ) -> Result<Option<Edge>, StorageError> {
        let namespace = namespace.to_string();
        let source_str = source_id.to_string();
        let target_str = target_id.to_string();
        let relation_str = relation.to_string();

        self.with_reader("get_edge_by_natural_key_including_deleted", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT namespace, id, source_id, target_id, relation, weight, \
                        created_at, updated_at, deleted_at, metadata, target_backend \
                 FROM graph_edges \
                 WHERE namespace = ?1 AND source_id = ?2 AND target_id = ?3 AND relation = ?4",
            )?;
            let mut rows = stmt.query(rusqlite::params![
                namespace,
                source_str,
                target_str,
                relation_str
            ])?;
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

    async fn get_edge_read_outcomes(
        &self,
        ids: &[LinkId],
    ) -> Result<Vec<Result<Option<Edge>, StorageError>>, StorageError> {
        const CHUNK: usize = 900;
        let mut outcomes = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(CHUNK) {
            let requested: Vec<String> =
                chunk.iter().map(|id| Uuid::from(*id).to_string()).collect();
            let expected = requested.len();
            let rows = self
                .with_reader("get_edge_read_outcomes", move |conn| {
                    let requested_json = serde_json::to_string(&requested)
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                    let mut stmt = conn.prepare(
                        "SELECT e.namespace, e.id, e.source_id, e.target_id, e.relation, e.weight, \
                                e.created_at, e.updated_at, e.deleted_at, e.metadata, e.target_backend, \
                                requested.key \
                         FROM json_each(?1) AS requested \
                         LEFT JOIN graph_edges AS e \
                           ON e.id = requested.value AND e.deleted_at IS NULL \
                         ORDER BY CAST(requested.key AS INTEGER) ASC",
                    )?;
                    let mut rows = stmt.query([requested_json])?;
                    let mut outcomes = Vec::with_capacity(expected);
                    while let Some(row) = rows.next()? {
                        let ordinal: i64 = row.get(11)?;
                        let ordinal =
                            usize::try_from(ordinal).map_err(|_| rusqlite::Error::InvalidQuery)?;
                        if ordinal != outcomes.len() {
                            return Err(rusqlite::Error::InvalidQuery);
                        }
                        let outcome = match row.get_ref(1)? {
                            rusqlite::types::ValueRef::Null => Ok(None),
                            _ => read_edge(row).map(Some).map_err(|e| map_err(e, "get_edge")),
                        };
                        outcomes.push(outcome);
                    }
                    if outcomes.len() != expected {
                        return Err(rusqlite::Error::InvalidQuery);
                    }
                    Ok(outcomes)
                })
                .await?;
            outcomes.extend(rows);
        }
        Ok(outcomes)
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
        // Declared across the whole chunk loop, not per chunk: a failure in a
        // later chunk must not erase what the earlier chunks already did.
        let counted_queries = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counted_rows = Arc::new(std::sync::atomic::AtomicU64::new(0));

        for chunk in unique_sources.chunks(CHUNK_SIZE) {
            let chunk_owned: Vec<Uuid> = chunk.to_vec();
            let query_clone = query.clone();
            let ns = namespace.clone();
            let closure_queries = Arc::clone(&counted_queries);
            let closure_rows = Arc::clone(&counted_rows);

            let chunk_result = self
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
                    closure_queries.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
                        closure_rows.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
                .await;
            let pairs = match chunk_result {
                Ok(pairs) => pairs,
                Err(e) => {
                    report_graph_usage(&counted_queries, &counted_rows);
                    return Err(e);
                }
            };
            result.extend(pairs);
        }
        report_graph_usage(&counted_queries, &counted_rows);

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
            let mut stmt = conn.prepare_cached(&statement.sql)?;
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
            let (where_clause, mut all_params) = build_edge_filter_sql(&namespace, &filter);
            let order_clause = edge_order_clause(&sort);
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

            Ok(Page { items, total: None })
        })
        .await
    }

    async fn count_edges(&self, filter: EdgeFilter) -> Result<u64, StorageError> {
        let namespace = self.namespace.clone();
        self.with_reader("count_edges", move |conn| {
            let (where_clause, params) = build_edge_filter_sql(&namespace, &filter);
            let sql = format!(
                "SELECT COUNT(*) FROM graph_edges{}",
                with_live_endpoints(&where_clause)
            );
            let mut stmt = conn.prepare(&sql)?;
            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(|p| p.as_ref()).collect();
            let count: i64 = stmt.query_row(param_refs.as_slice(), |row| row.get(0))?;
            Ok(count as u64)
        })
        .await
    }

    async fn count_edges_in_namespaces(
        &self,
        namespaces: &[String],
        filter: EdgeFilter,
    ) -> Result<u64, StorageError> {
        let namespaces: Vec<String> = namespaces
            .iter()
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        self.with_reader("count_edges_in_namespaces", move |conn| {
            let mut total = 0;
            for chunk in namespaces.chunks(NAMESPACE_COUNT_CHUNK_SIZE) {
                let (where_clause, params) = build_edge_filter_sql_for_namespaces(chunk, &filter);
                let sql = format!(
                    "SELECT COUNT(*) FROM graph_edges{}",
                    with_live_endpoints(&where_clause)
                );
                let mut stmt = conn.prepare(&sql)?;
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params.iter().map(|p| p.as_ref()).collect();
                let count: i64 = stmt.query_row(param_refs.as_slice(), |row| row.get(0))?;
                total += count as u64;
            }
            Ok(total)
        })
        .await
    }

    async fn query_edges_in_namespaces(
        &self,
        namespaces: &[String],
        filter: EdgeFilter,
        sort: Vec<SortOrder<EdgeSortField>>,
        page: PageRequest,
    ) -> Result<Page<Edge>, StorageError> {
        // One statement with `namespace IN (...)` and real SQL paging: a
        // per-namespace prefix fetch merged and re-sliced client-side floats
        // the offset window between calls, silently duplicating and skipping
        // rows during enumeration (#2088).
        //
        // The namespace set is bound as a single JSON-array parameter (see
        // `build_edge_filter_sql_for_namespaces_json`), not one `?N` per
        // namespace: a caller visible in hundreds-to-thousands of namespaces
        // would otherwise blow past `SQLITE_LIMIT_VARIABLE_NUMBER` before any
        // filter parameter is even added, and chunking the namespace set (as
        // the count-only aggregate methods below do) is not an option here —
        // it would reintroduce exactly the floating-offset bug this method
        // exists to close, since a per-chunk LIMIT/OFFSET cannot be
        // re-sliced into one globally exact page.
        let namespaces: Vec<String> = {
            let mut seen = HashSet::new();
            namespaces
                .iter()
                .filter(|ns| seen.insert((*ns).clone()))
                .cloned()
                .collect()
        };
        let limit_i64 = i64::from(page.limit);
        let offset_i64 = i64::try_from(page.offset).map_err(|_| StorageError::InvalidInput {
            capability: StorageCapability::Graph,
            operation: "query_edges_in_namespaces".into(),
            message: format!(
                "PageRequest: offset must be <= i64::MAX, got {}",
                page.offset
            ),
        })?;
        self.with_reader("query_edges_in_namespaces", move |conn| {
            let namespaces_json = serde_json::to_string(&namespaces)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;

            let (where_clause, mut all_params) =
                build_edge_filter_sql_for_namespaces_json(&namespaces_json, &filter);
            let order_clause = edge_order_clause(&sort);
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

            Ok(Page { items, total: None })
        })
        .await
    }

    async fn count_edges_by_relation(&self) -> Result<Vec<(EdgeRelation, u64)>, StorageError> {
        let namespace = self.namespace.clone();
        self.with_reader("count_edges_by_relation", move |conn| {
            let sql = format!(
                "SELECT relation, COUNT(*) FROM graph_edges \
                 WHERE namespace = ?1 AND deleted_at IS NULL AND {LIVE_ENDPOINTS_CONDITION} \
                 GROUP BY relation"
            );
            let mut stmt = conn.prepare(&sql)?;
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

    async fn count_edges_by_relation_in_namespaces(
        &self,
        namespaces: &[String],
    ) -> Result<Vec<(EdgeRelation, u64)>, StorageError> {
        let namespaces: Vec<String> = namespaces
            .iter()
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        self.with_reader("count_edges_by_relation_in_namespaces", move |conn| {
            let mut totals = HashMap::new();
            for chunk in namespaces.chunks(NAMESPACE_COUNT_CHUNK_SIZE) {
                let (where_clause, params) =
                    build_edge_filter_sql_for_namespaces(chunk, &EdgeFilter::default());
                let sql = format!(
                    "SELECT relation, COUNT(*) FROM graph_edges{} GROUP BY relation",
                    with_live_endpoints(&where_clause)
                );
                let mut stmt = conn.prepare(&sql)?;
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params.iter().map(|p| p.as_ref()).collect();
                let rows = stmt.query_map(param_refs.as_slice(), |row| {
                    let relation_str: String = row.get(0)?;
                    let count: i64 = row.get(1)?;
                    Ok((relation_str, count))
                })?;
                for row in rows {
                    let (relation_str, count) = row?;
                    let relation = relation_str.parse::<EdgeRelation>().map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?;
                    *totals.entry(relation).or_insert(0) += count as u64;
                }
            }
            Ok(totals.into_iter().collect())
        })
        .await
    }

    async fn count_edges_by_endpoint_base(&self) -> Result<EdgeEndpointBaseCounts, StorageError> {
        let namespace = self.namespace.clone();
        self.with_reader("count_edges_by_endpoint_base", move |conn| {
            let source_case = endpoint_base_case("source_id");
            let target_case = endpoint_base_case("target_id");
            let sql = format!(
                "SELECT {source_case} AS source_base, {target_case} AS target_base, COUNT(*) \
                 FROM graph_edges \
                 WHERE namespace = ?1 AND deleted_at IS NULL AND {LIVE_ENDPOINTS_CONDITION} \
                 GROUP BY source_base, target_base"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map([&namespace], |row| {
                let source: String = row.get(0)?;
                let target: String = row.get(1)?;
                let count: i64 = row.get(2)?;
                Ok((source, target, count))
            })?;
            let mut counts = EdgeEndpointBaseCounts::default();
            for row in rows {
                let (source, target, count) = row?;
                fold_endpoint_base_row(&mut counts, &source, &target, count as u64);
            }
            Ok(counts)
        })
        .await
    }

    async fn count_edges_by_endpoint_base_in_namespaces(
        &self,
        namespaces: &[String],
    ) -> Result<EdgeEndpointBaseCounts, StorageError> {
        let namespaces: Vec<String> = namespaces
            .iter()
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        self.with_reader("count_edges_by_endpoint_base_in_namespaces", move |conn| {
            let source_case = endpoint_base_case("source_id");
            let target_case = endpoint_base_case("target_id");
            let mut counts = EdgeEndpointBaseCounts::default();
            for chunk in namespaces.chunks(NAMESPACE_COUNT_CHUNK_SIZE) {
                let (where_clause, params) =
                    build_edge_filter_sql_for_namespaces(chunk, &EdgeFilter::default());
                let sql = format!(
                    "SELECT {source_case} AS source_base, {target_case} AS target_base, COUNT(*) \
                     FROM graph_edges{} GROUP BY source_base, target_base",
                    with_live_endpoints(&where_clause)
                );
                let mut stmt = conn.prepare(&sql)?;
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params.iter().map(|p| p.as_ref()).collect();
                let rows = stmt.query_map(param_refs.as_slice(), |row| {
                    let source: String = row.get(0)?;
                    let target: String = row.get(1)?;
                    let count: i64 = row.get(2)?;
                    Ok((source, target, count))
                })?;
                for row in rows {
                    let (source, target, count) = row?;
                    fold_endpoint_base_row(&mut counts, &source, &target, count as u64);
                }
            }
            Ok(counts)
        })
        .await
    }

    async fn count_edges_by_relation_and_endpoint_base_in_namespaces(
        &self,
        namespaces: &[String],
    ) -> Result<Vec<(EdgeRelation, EdgeEndpointBaseCounts)>, StorageError> {
        let namespaces: Vec<String> = namespaces
            .iter()
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        self.with_reader(
            "count_edges_by_relation_and_endpoint_base_in_namespaces",
            move |conn| {
                let mut totals: HashMap<EdgeRelation, EdgeEndpointBaseCounts> = HashMap::new();
                for chunk in namespaces.chunks(NAMESPACE_COUNT_CHUNK_SIZE) {
                    let (where_clause, params) =
                        build_edge_filter_sql_for_namespaces(chunk, &EdgeFilter::default());
                    // One lookup per endpoint per table answers both questions the
                    // single-purpose counts ask with separate correlated subqueries:
                    // liveness as in [`LIVE_ENDPOINTS_CONDITION`] (an endpoint is
                    // excluded only when present and tombstoned) and the base as in
                    // [`endpoint_base_case`] (entity first, then note, else none). `id`
                    // is the primary key of both tables, so each LEFT JOIN matches at
                    // most one row and never multiplies an edge. The edge filter runs
                    // in the inner select, where its unqualified columns resolve
                    // against graph_edges alone.
                    let sql = format!(
                        "SELECT e.relation, \
                         CASE WHEN se.id IS NOT NULL THEN 'entity' \
                         WHEN sn.id IS NOT NULL THEN 'note' ELSE 'none' END AS source_base, \
                         CASE WHEN te.id IS NOT NULL THEN 'entity' \
                         WHEN tn.id IS NOT NULL THEN 'note' ELSE 'none' END AS target_base, \
                         COUNT(*) \
                         FROM (SELECT relation, source_id, target_id \
                         FROM graph_edges{where_clause}) e \
                         LEFT JOIN entities se ON se.id = e.source_id \
                         LEFT JOIN notes sn ON sn.id = e.source_id \
                         LEFT JOIN entities te ON te.id = e.target_id \
                         LEFT JOIN notes tn ON tn.id = e.target_id \
                         WHERE se.deleted_at IS NULL AND sn.deleted_at IS NULL \
                         AND te.deleted_at IS NULL AND tn.deleted_at IS NULL \
                         GROUP BY e.relation, source_base, target_base"
                    );
                    let mut stmt = conn.prepare(&sql)?;
                    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                        params.iter().map(|p| p.as_ref()).collect();
                    let rows = stmt.query_map(param_refs.as_slice(), |row| {
                        let relation: String = row.get(0)?;
                        let source: String = row.get(1)?;
                        let target: String = row.get(2)?;
                        let count: i64 = row.get(3)?;
                        Ok((relation, source, target, count))
                    })?;
                    for row in rows {
                        let (relation_str, source, target, count) = row?;
                        let relation = relation_str.parse::<EdgeRelation>().map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })?;
                        fold_endpoint_base_row(
                            totals.entry(relation).or_default(),
                            &source,
                            &target,
                            count as u64,
                        );
                    }
                }
                Ok(totals.into_iter().collect())
            },
        )
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

    async fn query_edges_sequence_after(
        &self,
        filter: EdgeFilter,
        after: Option<SeekCursor>,
        limit: u32,
    ) -> Result<SeekPage<Edge>, StorageError> {
        if limit == 0 {
            return Ok(SeekPage::default());
        }
        let namespace = self.namespace.clone();
        let limit_usize = limit as usize;
        let probe_limit_i64 = i64::from(limit) + 1;
        self.with_reader("query_edges_sequence_after", move |conn| {
            let (mut where_clause, mut params) = build_edge_filter_sql(&namespace, &filter);
            if let Some(cursor) = after {
                params.push(Box::new(cursor.sequence));
                where_clause.push_str(&format!(" AND graph_edges_seq.seq > ?{}", params.len()));
            }
            params.push(Box::new(probe_limit_i64));
            let limit_idx = params.len();
            // CROSS JOIN fixes the ledger as the outer loop, preserving an
            // indexed `seq > boundary` scan with no full-match sort.
            let sql = format!(
                "SELECT namespace, id, source_id, target_id, relation, weight, \
                        created_at, updated_at, deleted_at, metadata, target_backend, \
                        graph_edges_seq.seq \
                 FROM graph_edges_seq CROSS JOIN graph_edges \
                    ON graph_edges.id = graph_edges_seq.edge_id{where_clause} \
                 ORDER BY graph_edges_seq.seq ASC LIMIT ?{limit_idx}"
            );
            let mut stmt = conn.prepare(&sql)?;
            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(|param| param.as_ref()).collect();
            let rows = stmt.query_map(param_refs.as_slice(), |row| {
                Ok((read_edge(row)?, row.get::<_, i64>(11)?))
            })?;
            let mut entries = rows.collect::<Result<Vec<_>, _>>()?;
            let has_more = entries.len() > limit_usize;
            if has_more {
                entries.truncate(limit_usize);
            }
            let next_after = if has_more {
                entries.last().map(|(edge, sequence)| SeekCursor {
                    sequence: *sequence,
                    id: Uuid::from(edge.id),
                })
            } else {
                None
            };
            let items = entries.into_iter().map(|(edge, _)| edge).collect();
            Ok(SeekPage { items, next_after })
        })
        .await
    }

    async fn neighbors(
        &self,
        node_id: Uuid,
        query: NeighborQuery,
    ) -> Result<Vec<NeighborHit>, StorageError> {
        self.query_neighbors_page("neighbors", node_id, query, None, None)
            .await
    }

    async fn neighbors_page(
        &self,
        node_id: Uuid,
        query: NeighborQuery,
        after: Option<NeighborCursor>,
        neighbor_kinds: Option<Vec<String>>,
    ) -> Result<Vec<NeighborHit>, StorageError> {
        self.query_neighbors_page("neighbors_page", node_id, query, after, neighbor_kinds)
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

        let counted_queries = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counted_rows = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let closure_queries = Arc::clone(&counted_queries);
        let closure_rows = Arc::clone(&counted_rows);
        let result = self
            .with_reader("neighbors_both_directions", move |conn| {
                let base_out = "SELECT target_id AS node_id, id AS edge_id, relation, weight, \
                            'out' AS dir \
                            FROM graph_edges \
                            WHERE namespace = ?1 AND source_id = ?2 AND deleted_at IS NULL";
                let base_in = "SELECT source_id AS node_id, id AS edge_id, relation, weight, \
                           'in' AS dir \
                           FROM graph_edges \
                           WHERE namespace = ?1 AND target_id = ?2 AND deleted_at IS NULL";
                let sql = format!("{} UNION ALL {}", base_out, base_in);

                let (where_extra, limit_clause, extra_params) =
                    neighbor_extra_clause(&query, 3, None, None);

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

                closure_queries.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
                    closure_rows.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
            .await;

        report_graph_usage(&counted_queries, &counted_rows);
        result
    }

    async fn traverse(&self, request: TraversalRequest) -> Result<Vec<GraphPath>, StorageError> {
        request
            .validate()
            .map_err(|message| StorageError::InvalidInput {
                capability: StorageCapability::Graph,
                operation: "traverse".into(),
                message,
            })?;
        if request.roots.is_empty() {
            return Ok(Vec::new());
        }

        let budget = request.execution_budget;
        let outer_context = khive_storage::capture_request_read_context();
        let execution_deadline =
            khive_storage::RequestReadDeadline::after(budget.remaining_duration());
        khive_storage::scope_request_read_deadline_at(execution_deadline, async {
            if outer_context.stop_reason().is_some() {
                return Err(StorageError::Timeout {
                    operation: "traverse".into(),
                });
            }
            if budget.is_expired() {
                return Err(traversal_timeout_error(&budget));
            }
            let mut distinct_roots = HashSet::with_capacity(request.roots.len());
            let roots = request
                .roots
                .iter()
                .copied()
                .filter(|root| distinct_roots.insert(*root))
                .collect::<Vec<_>>();
            let opts = request.options;
            if self
                .index_repair
                .as_ref()
                .is_some_and(crate::stores::index_repair::IndexRepairContext::is_writable)
            {
                // Prepare and step the actual indexed adjacency statements with
                // LIMIT 0 before BFS takes its long-lived reader. This checks the
                // schema cookie without consuming walk rows or query counters.
                // A schema change during the walk remains its original typed
                // failure; we never restart BFS or renew its budget.
                let directions = match opts.direction {
                    Direction::Out => vec![Direction::Out],
                    Direction::In => vec![Direction::In],
                    Direction::Both => vec![Direction::Out, Direction::In],
                };
                let relation_count = opts.relations.as_ref().map_or(0, Vec::len);
                let statements = directions
                    .into_iter()
                    .map(|direction| {
                        traversal_neighbor_sql(direction, relation_count, opts.min_weight.is_some())
                    })
                    .collect::<Vec<_>>();
                // Dropping preflight stops pending repair admission. Already admitted
                // constructor DDL keeps its completion ownership outside this await.
                let preflight = self.with_indexed_reader("traverse", move |conn| {
                    for sql in &statements {
                        let mut statement = conn.prepare(sql)?;
                        statement.raw_bind_parameter(3, 0_i64)?;
                        let _ = statement.raw_query().next()?;
                    }
                    Ok(())
                });
                let preflight_result = tokio::select! {
                    biased;
                    _ = outer_context.clone().wait_for_stop() => Err(StorageError::Timeout {
                        operation: "traverse".into(),
                    }),
                    result = tokio::time::timeout_at(execution_deadline.async_at(), preflight) => {
                        match result {
                            Ok(result) => result,
                            Err(_) => Err(traversal_timeout_error(&budget)),
                        }
                    },
                };
                preflight_result.map_err(|error| match error {
                    StorageError::Timeout { .. }
                        if budget.is_expired() && outer_context.stop_reason().is_none() =>
                    {
                        traversal_timeout_error(&budget)
                    }
                    error => error,
                })?;
                if budget.is_expired() {
                    return Err(traversal_timeout_error(&budget));
                }
            }
            let include_roots = request.include_roots;
            let namespace = self.namespace.clone();
            let origin = self.pool.origin();
            let closure_budget = budget.clone();
            // Shared with the blocking closure so the counts survive an error.
            // `with_reader` runs the closure on a blocking thread where the
            // task-local usage context is invisible, so it cannot call
            // `usage::count` itself; returning the totals in the Ok value would
            // lose every round trip already issued when a later statement fails.
            let counted_rows = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let counted_queries = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let closure_rows = Arc::clone(&counted_rows);
            let closure_queries = Arc::clone(&counted_queries);
            let result = self
                .with_reader("traverse", move |conn| {
                    Ok(run_bounded_traversal(
                        conn,
                        roots,
                        opts,
                        include_roots,
                        namespace,
                        origin,
                        closure_budget,
                        closure_rows.as_ref(),
                        closure_queries.as_ref(),
                    ))
                })
                .await
                .and_then(|inner| inner)
                .map_err(|error| match error {
                    StorageError::Timeout { .. }
                        if budget.is_expired() && outer_context.stop_reason().is_none() =>
                    {
                        traversal_timeout_error(&budget)
                    }
                    error => error,
                });

            // Accounted on BOTH outcomes. `db_round_trips` counts round trips
            // *issued* and `graph_hops` counts adjacency rows storage *returned*,
            // so work already done before a later statement errors is real work and
            // must appear. Reading the shared counters here rather than off the
            // Ok value is what makes that possible: the closure runs on a blocking
            // thread where the task-local usage context is not visible, so it
            // cannot count for itself, and a value returned only on success
            // reports nothing at all when the traversal fails partway.
            khive_storage::usage::count(
                khive_storage::usage::UsageUnit::DbRoundTrips,
                counted_queries.load(std::sync::atomic::Ordering::Relaxed),
            );
            khive_storage::usage::count(
                khive_storage::usage::UsageUnit::GraphHops,
                counted_rows.load(std::sync::atomic::Ordering::Relaxed),
            );

            result
        })
        .await
    }

    async fn purge_incident_edges(&self, node_id: Uuid) -> Result<u64, StorageError> {
        // No namespace filter: UUID v4 is globally unique. Hard-delete cascade must
        // remove ALL incident edges regardless of which namespace they were written in
        // (ADR-002 no-dangling-references, ADR-007 by-ID contract).
        let statement = purge_incident_edges_statement(node_id);
        self.with_writer("purge_incident_edges", move |conn| {
            let mut stmt = conn.prepare_cached(&statement.sql)?;
            bind_params(&mut stmt, &statement.params)?;
            Ok(stmt.raw_execute()? as u64)
        })
        .await
    }
}
