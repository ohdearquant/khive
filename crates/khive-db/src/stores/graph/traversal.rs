use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use khive_storage::types::{
    Direction, GraphPath, PathNode, TraversalExecutionBudget, TraversalOptions,
};
use khive_storage::{StorageCapability, StorageError};
use uuid::Uuid;

use super::edge_reads::parse_uuid;
use super::map_err;
#[cfg(test)]
use super::tests;

/// One index-seek adjacency statement used by bounded level-synchronous BFS.
/// There is deliberately no `ORDER BY`: the public contract promises minimum
/// depth, not a same-depth tie order, and sorting a high-degree node before a
/// low `LIMIT` would reintroduce the unbounded SQL work fixed by #1444.
pub(super) fn traversal_neighbor_sql(
    direction: Direction,
    relation_count: usize,
    has_min_weight: bool,
) -> String {
    let (node_column, endpoint_column, index) = match direction {
        Direction::Out => ("target_id", "source_id", "idx_graph_edges_ns_src_rel"),
        Direction::In => ("source_id", "target_id", "idx_graph_edges_ns_tgt_rel"),
        Direction::Both => unreachable!("Direction::Both is split into indexed Out/In seeks"),
    };
    let mut sql = format!(
        "SELECT {node_column}, id, weight \
         FROM graph_edges INDEXED BY {index} \
         WHERE namespace = ?1 AND {endpoint_column} = ?2 AND deleted_at IS NULL"
    );
    if relation_count > 0 {
        let placeholders = (0..relation_count)
            .map(|offset| format!("?{}", 4 + offset))
            .collect::<Vec<_>>()
            .join(",");
        sql.push_str(&format!(" AND relation IN ({placeholders})"));
    }
    if has_min_weight {
        sql.push_str(&format!(" AND weight >= ?{}", 4 + relation_count));
    }
    sql.push_str(" LIMIT ?3");
    sql
}

pub(super) fn traversal_timeout_error(budget: &TraversalExecutionBudget) -> StorageError {
    StorageError::Timeout {
        operation: format!(
            "traverse ({}ms execution budget)",
            budget.max_duration().as_millis()
        )
        .into(),
    }
}

pub(super) fn traversal_work_error(budget: &TraversalExecutionBudget) -> StorageError {
    StorageError::InvalidInput {
        capability: StorageCapability::Graph,
        operation: "traverse".into(),
        message: format!(
            "traversal work budget exceeded after {} adjacency rows; \
             narrow roots, depth, relations, or result limit",
            budget.work_limit()
        ),
    }
}

#[derive(Clone, Copy)]
pub(super) struct TraversalFrontierNode {
    pub(super) node_id: Uuid,
    pub(super) depth: usize,
    pub(super) total_weight: f64,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_bounded_traversal(
    conn: &rusqlite::Connection,
    roots: Vec<Uuid>,
    opts: TraversalOptions,
    include_roots: bool,
    namespace: String,
    origin: khive_storage::tx_registry::TxOrigin,
    budget: TraversalExecutionBudget,
    counted_rows: &std::sync::atomic::AtomicU64,
    counted_queries: &std::sync::atomic::AtomicU64,
) -> Result<Vec<GraphPath>, StorageError> {
    let progress_timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let callback_timed_out = Arc::clone(&progress_timed_out);
    let callback_budget = budget.clone();
    #[cfg(test)]
    let progress_seam_root = roots.first().copied();
    conn.progress_handler(
        1_000,
        Some(move || {
            if crate::read_cancellation::current_read_should_interrupt() {
                return true;
            }
            #[cfg(test)]
            if tests::traverse_progress_seam::hook(progress_seam_root) {
                callback_timed_out.store(true, std::sync::atomic::Ordering::Relaxed);
                return true;
            }
            let expired = callback_budget.is_expired();
            if expired {
                callback_timed_out.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            expired
        }),
    )
    .map_err(|e| map_err(e, "traverse_progress_handler"))?;

    let result = (|| {
        let result_limit = opts.effective_limit() as usize;
        let relation_count = opts.relations.as_ref().map_or(0, Vec::len);
        let directions = match opts.direction {
            Direction::Out => vec![Direction::Out],
            Direction::In => vec![Direction::In],
            Direction::Both => vec![Direction::Out, Direction::In],
        };
        let statements = directions
            .into_iter()
            .map(|direction| {
                traversal_neighbor_sql(direction, relation_count, opts.min_weight.is_some())
            })
            .collect::<Vec<_>>();
        let map_sql_error = |error| {
            if progress_timed_out.load(std::sync::atomic::Ordering::Relaxed) {
                traversal_timeout_error(&budget)
            } else {
                map_err(error, "traverse")
            }
        };

        let mut all_paths = Vec::with_capacity(roots.len());
        for root_id in roots {
            let mut seen = HashSet::new();
            seen.insert(root_id);
            let mut frontier = VecDeque::from([TraversalFrontierNode {
                node_id: root_id,
                depth: 0,
                total_weight: 0.0,
            }]);
            let mut nodes = Vec::with_capacity(result_limit + usize::from(include_roots));
            if include_roots {
                nodes.push(PathNode {
                    node_id: root_id,
                    via_edge: None,
                    depth: 0,
                    name: None,
                    kind: None,
                    properties: None,
                    weight: 0.0,
                });
            }
            let mut non_root_count = 0usize;

            'root_walk: while non_root_count < result_limit {
                let Some(current) = frontier.pop_front() else {
                    break;
                };
                if current.depth >= opts.max_depth {
                    continue;
                }
                if budget.is_expired() {
                    return Err(traversal_timeout_error(&budget));
                }

                for sql in &statements {
                    let row_cap = budget.remaining_work().saturating_add(1);
                    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![
                        Box::new(namespace.clone()),
                        Box::new(current.node_id.to_string()),
                        Box::new(row_cap as i64),
                    ];
                    if let Some(relations) = &opts.relations {
                        params.extend(relations.iter().map(|relation| {
                            Box::new(relation.to_string()) as Box<dyn rusqlite::types::ToSql>
                        }));
                    }
                    if let Some(min_weight) = opts.min_weight {
                        params.push(Box::new(min_weight));
                    }
                    let param_refs = params
                        .iter()
                        .map(|param| param.as_ref())
                        .collect::<Vec<&dyn rusqlite::types::ToSql>>();

                    let _snapshot = khive_storage::tx_registry::register_scoped(
                        Some("graph_traverse_read".to_string()),
                        origin.clone(),
                    );
                    let mut stmt = conn.prepare(sql).map_err(&map_sql_error)?;
                    counted_queries.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let mut rows = stmt.query(param_refs.as_slice()).map_err(&map_sql_error)?;
                    while let Some(row) = rows.next().map_err(&map_sql_error)? {
                        // Test seam after `sqlite3_step` has produced a row:
                        // the cursor's read snapshot is demonstrably live.
                        #[cfg(test)]
                        tests::traverse_snapshot_seam::hook(current.node_id);
                        counted_rows.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if budget.is_expired() {
                            return Err(traversal_timeout_error(&budget));
                        }
                        if !budget.try_consume_row() {
                            return Err(traversal_work_error(&budget));
                        }
                        let node_str: String = row.get(0).map_err(&map_sql_error)?;
                        let edge_str: String = row.get(1).map_err(&map_sql_error)?;
                        let edge_weight: f64 = row.get(2).map_err(&map_sql_error)?;
                        let node_id = parse_uuid(&node_str).map_err(&map_sql_error)?;
                        if !seen.insert(node_id) {
                            continue;
                        }
                        let via_edge = parse_uuid(&edge_str).map_err(&map_sql_error)?;
                        let depth = current.depth + 1;
                        let total_weight = current.total_weight + edge_weight;
                        nodes.push(PathNode {
                            node_id,
                            via_edge: Some(via_edge),
                            depth,
                            name: None,
                            kind: None,
                            properties: None,
                            weight: total_weight,
                        });
                        non_root_count += 1;
                        if depth < opts.max_depth {
                            frontier.push_back(TraversalFrontierNode {
                                node_id,
                                depth,
                                total_weight,
                            });
                        }
                        if non_root_count == result_limit {
                            break 'root_walk;
                        }
                    }
                }
            }

            if !nodes.is_empty() {
                let total_weight = nodes.iter().map(|node| node.weight).fold(0.0_f64, f64::max);
                all_paths.push(GraphPath {
                    root_id,
                    nodes,
                    total_weight,
                });
            }
        }
        Ok(all_paths)
    })();

    conn.progress_handler(0, None::<fn() -> bool>)
        .map_err(|e| map_err(e, "traverse_progress_handler_clear"))?;
    result
}
