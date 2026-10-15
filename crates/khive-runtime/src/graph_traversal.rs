// Copyright 2024-2025 Haiyang Li
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use khive_storage::types::{Direction, Edge, LinkId, NeighborQuery, TraversalOptions};

use crate::error::{RuntimeError, RuntimeResult};
use crate::runtime::{KhiveRuntime, NamespaceToken};

/// A node in a traversal path.
#[derive(Debug, Clone)]
pub struct PathNode {
    /// Entity at this position.
    pub entity_id: Uuid,
    /// Distance from the start node (0 = start node).
    pub depth: usize,
    /// Edge that led to this node (`None` for the start node).
    pub via_edge: Option<Edge>,
}

impl KhiveRuntime {
    /// BFS traversal from `start`, returning nodes in level order.
    ///
    /// The first element is always the start node (`via_edge = None`, `depth = 0`).
    /// Nodes already visited are skipped so the result set is deduplicated.
    ///
    /// DB round-trips are O(max_depth): one `batch_neighbors` call and one
    /// `get_edges` call per BFS level, rather than one call per node/edge.
    pub async fn bfs_traverse(
        &self,
        token: &NamespaceToken,
        start: Uuid,
        options: TraversalOptions,
    ) -> RuntimeResult<Vec<PathNode>> {
        if !self.substrate_exists_in_ns(token, start).await? {
            return Ok(Vec::new());
        }

        let graph = self.graph(token)?;
        let limit = options.limit.map(|n| n as usize).unwrap_or(usize::MAX);

        let mut visited: HashSet<Uuid> = HashSet::new();
        let mut results: Vec<PathNode> = Vec::new();
        let mut frontier: Vec<Uuid> = Vec::new();

        visited.insert(start);
        results.push(PathNode {
            entity_id: start,
            depth: 0,
            via_edge: None,
        });
        frontier.push(start);

        let mut depth = 0usize;

        'bfs: while !frontier.is_empty() && depth < options.max_depth {
            let query = NeighborQuery {
                direction: options.direction.clone(),
                relations: options.relations.clone(),
                limit: None,
                min_weight: None,
            };

            // One DB round-trip for all nodes in the current frontier.
            let all_hits = graph.batch_neighbors(&frontier, query).await?;

            let mut level_new: Vec<(Uuid, Uuid)> = Vec::new();
            for (_src, hit) in &all_hits {
                if visited.contains(&hit.node_id) {
                    continue;
                }
                // Mark visited before pushing so duplicate edges in the same level don't duplicate PathNodes.
                if visited.insert(hit.node_id) {
                    level_new.push((hit.node_id, hit.edge_id));
                }
            }

            if level_new.is_empty() {
                break 'bfs;
            }

            // One DB round-trip to fetch all edges for this level.
            let edge_ids: Vec<LinkId> = level_new
                .iter()
                .map(|(_, eid)| LinkId::from(*eid))
                .collect();
            let edges = graph.get_edges(&edge_ids).await?;
            let edge_map: HashMap<Uuid, Edge> =
                edges.into_iter().map(|e| (Uuid::from(e.id), e)).collect();

            let next_depth = depth + 1;
            frontier.clear();
            for (node_id, edge_id) in level_new {
                let via_edge = edge_map.get(&edge_id).cloned().or(None);
                // A missing edge here means it was soft-deleted between the neighbors and get_edges calls;
                // error out rather than silently returning an incomplete path.
                if via_edge.is_none() {
                    return Err(RuntimeError::NotFound(format!("edge {} missing", edge_id)));
                }
                results.push(PathNode {
                    entity_id: node_id,
                    depth: next_depth,
                    via_edge,
                });
                if results.len() >= limit {
                    break 'bfs;
                }
                frontier.push(node_id);
            }

            depth = next_depth;
        }

        Ok(results)
    }

    /// Bidirectional BFS shortest path from `from` to `to`.
    ///
    /// Returns `Some(path)` where `path[0]` is `from` and `path.last()` is `to`,
    /// or `None` if no path exists within `max_depth` hops.
    /// For `from == to` returns `Some` with a single-node path immediately.
    ///
    /// DB round-trips are O(max_depth): one `batch_neighbors` per frontier
    /// expansion level, plus one `get_edges` call during path reconstruction.
    pub async fn shortest_path(
        &self,
        token: &NamespaceToken,
        from: Uuid,
        to: Uuid,
        max_depth: usize,
    ) -> RuntimeResult<Option<Vec<PathNode>>> {
        if !self.substrate_exists_in_ns(token, from).await?
            || !self.substrate_exists_in_ns(token, to).await?
        {
            return Ok(None);
        }

        if from == to {
            return Ok(Some(vec![PathNode {
                entity_id: from,
                depth: 0,
                via_edge: None,
            }]));
        }

        let graph = self.graph(token)?;

        // Forward map: node -> (depth, parent, edge_id that reached this node)
        let mut fwd: HashMap<Uuid, (usize, Option<Uuid>, Option<Uuid>)> = HashMap::new();
        let mut fwd_frontier: Vec<Uuid> = vec![from];
        fwd.insert(from, (0, None, None));

        // Backward map: node -> (depth, child, edge_id that reached this node from `to` side)
        let mut bwd: HashMap<Uuid, (usize, Option<Uuid>, Option<Uuid>)> = HashMap::new();
        let mut bwd_frontier: Vec<Uuid> = vec![to];
        bwd.insert(to, (0, None, None));

        let mut meeting: Option<(Uuid, usize)> = None;
        let mut current_depth = 0usize;

        while (!fwd_frontier.is_empty() || !bwd_frontier.is_empty()) && current_depth <= max_depth {
            if !fwd_frontier.is_empty() {
                let hits = graph
                    .batch_neighbors(
                        &fwd_frontier,
                        NeighborQuery {
                            direction: Direction::Out,
                            relations: None,
                            limit: None,
                            min_weight: None,
                        },
                    )
                    .await?;

                let mut next_fwd: Vec<Uuid> = Vec::new();
                for (src, hit) in &hits {
                    if fwd.contains_key(&hit.node_id) {
                        continue;
                    }
                    let new_depth = fwd[src].0 + 1;
                    fwd.insert(hit.node_id, (new_depth, Some(*src), Some(hit.edge_id)));
                    next_fwd.push(hit.node_id);

                    if let Some(&(bwd_depth, _, _)) = bwd.get(&hit.node_id) {
                        let total = new_depth + bwd_depth;
                        if total <= max_depth
                            && meeting.as_ref().is_none_or(|&(_, best)| total < best)
                        {
                            meeting = Some((hit.node_id, total));
                        }
                    }
                }
                fwd_frontier = next_fwd;
            }

            if meeting.is_some() {
                break;
            }

            if !bwd_frontier.is_empty() {
                let hits = graph
                    .batch_neighbors(
                        &bwd_frontier,
                        NeighborQuery {
                            direction: Direction::In,
                            relations: None,
                            limit: None,
                            min_weight: None,
                        },
                    )
                    .await?;

                let mut next_bwd: Vec<Uuid> = Vec::new();
                for (src, hit) in &hits {
                    if bwd.contains_key(&hit.node_id) {
                        continue;
                    }
                    let new_depth = bwd[src].0 + 1;
                    bwd.insert(hit.node_id, (new_depth, Some(*src), Some(hit.edge_id)));
                    next_bwd.push(hit.node_id);

                    if let Some(&(fwd_depth, _, _)) = fwd.get(&hit.node_id) {
                        let total = fwd_depth + new_depth;
                        if total <= max_depth
                            && meeting.as_ref().is_none_or(|&(_, best)| total < best)
                        {
                            meeting = Some((hit.node_id, total));
                        }
                    }
                }
                bwd_frontier = next_bwd;
            }

            if meeting.is_some() {
                break;
            }

            current_depth += 1;
        }

        let (mid, _) = match meeting {
            None => return Ok(None),
            Some(m) => m,
        };

        let mut fwd_chain: Vec<(Uuid, Option<Uuid>)> = Vec::new();
        {
            let mut cur = mid;
            loop {
                let (_, parent, edge_id) = fwd[&cur];
                fwd_chain.push((cur, edge_id));
                match parent {
                    Some(p) => cur = p,
                    None => break,
                }
            }
        }
        fwd_chain.reverse();

        let mut bwd_chain: Vec<(Uuid, Option<Uuid>)> = Vec::new();
        {
            let mut cur = mid;
            while let Some(&(_, Some(child), edge_id)) = bwd.get(&cur) {
                bwd_chain.push((child, edge_id));
                cur = child;
            }
        }

        // Fetch all path edges in one batch call rather than one round-trip per edge.
        let path_edge_ids: Vec<LinkId> = fwd_chain
            .iter()
            .chain(bwd_chain.iter())
            .filter_map(|(_, eid)| eid.map(LinkId::from))
            .collect();

        let path_edges = graph.get_edges(&path_edge_ids).await?;
        let edge_map: HashMap<Uuid, Edge> = path_edges
            .into_iter()
            .map(|e| (Uuid::from(e.id), e))
            .collect();

        let mut path: Vec<PathNode> = Vec::new();
        for (i, (node_id, edge_id)) in fwd_chain.iter().enumerate() {
            let via_edge = if i == 0 {
                None // start node
            } else {
                edge_id.and_then(|eid| edge_map.get(&eid).cloned())
            };
            path.push(PathNode {
                entity_id: *node_id,
                depth: i,
                via_edge,
            });
        }

        let base = path.len();
        for (i, (node_id, edge_id)) in bwd_chain.iter().enumerate() {
            let via_edge = edge_id.and_then(|eid| edge_map.get(&eid).cloned());
            path.push(PathNode {
                entity_id: *node_id,
                depth: base + i,
                via_edge,
            });
        }

        Ok(Some(path))
    }
}

// Kept inline (not in tests/) because these tests exercise private traversal state that
// would otherwise need to be made pub.
#[cfg(test)]
#[path = "graph_traversal_tests.rs"]
mod tests;
