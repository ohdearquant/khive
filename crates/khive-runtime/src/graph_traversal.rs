// Copyright 2024-2025 khive contributors
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

use std::collections::{HashMap, HashSet, VecDeque};

use uuid::Uuid;

use khive_storage::types::{Direction, Edge, LinkId, NeighborQuery};
use khive_storage::EdgeRelation;

use crate::error::{RuntimeError, RuntimeResult};
use crate::runtime::KhiveRuntime;

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

/// Options for BFS traversal and shortest-path search.
#[derive(Debug, Clone)]
pub struct TraversalOptions {
    /// Maximum hops to follow.
    pub max_depth: usize,
    /// Which edge directions to follow.
    pub direction: Direction,
    /// Restrict traversal to these relation types (`None` = all).
    pub relations: Option<Vec<EdgeRelation>>,
    /// Stop after collecting this many nodes (start node counts as one).
    pub max_results: Option<usize>,
}

impl Default for TraversalOptions {
    fn default() -> Self {
        Self {
            max_depth: 3,
            direction: Direction::Out,
            relations: None,
            max_results: None,
        }
    }
}

impl KhiveRuntime {
    /// BFS traversal from `start`, returning nodes in level order.
    ///
    /// The first element is always the start node (`via_edge = None`, `depth = 0`).
    /// Nodes already visited are skipped so the result set is deduplicated.
    pub async fn bfs_traverse(
        &self,
        namespace: Option<&str>,
        start: Uuid,
        options: TraversalOptions,
    ) -> RuntimeResult<Vec<PathNode>> {
        let graph = self.graph(namespace)?;
        let limit = options.max_results.unwrap_or(usize::MAX);

        let mut visited: HashSet<Uuid> = HashSet::new();
        let mut results: Vec<PathNode> = Vec::new();
        // queue: (node_id, current_depth)
        let mut queue: VecDeque<(Uuid, usize)> = VecDeque::new();

        visited.insert(start);
        results.push(PathNode {
            entity_id: start,
            depth: 0,
            via_edge: None,
        });
        queue.push_back((start, 0));

        'bfs: while let Some((current, depth)) = queue.pop_front() {
            if depth >= options.max_depth {
                continue;
            }

            let query = NeighborQuery {
                direction: options.direction.clone(),
                relations: options.relations.clone(),
                limit: None,
                min_weight: None,
            };
            let hits = graph.neighbors(current, query).await?;

            for hit in hits {
                if visited.contains(&hit.node_id) {
                    continue;
                }

                let edge = graph
                    .get_edge(LinkId::from(hit.edge_id))
                    .await?
                    .ok_or_else(|| {
                        RuntimeError::NotFound(format!("edge {} missing", hit.edge_id))
                    })?;

                visited.insert(hit.node_id);
                results.push(PathNode {
                    entity_id: hit.node_id,
                    depth: depth + 1,
                    via_edge: Some(edge),
                });

                if results.len() >= limit {
                    break 'bfs;
                }

                queue.push_back((hit.node_id, depth + 1));
            }
        }

        Ok(results)
    }

    /// Bidirectional BFS shortest path from `from` to `to`.
    ///
    /// Returns `Some(path)` where `path[0]` is `from` and `path.last()` is `to`,
    /// or `None` if no path exists within `max_depth` hops.
    /// For `from == to` returns `Some` with a single-node path immediately.
    pub async fn shortest_path(
        &self,
        namespace: Option<&str>,
        from: Uuid,
        to: Uuid,
        max_depth: usize,
    ) -> RuntimeResult<Option<Vec<PathNode>>> {
        if from == to {
            return Ok(Some(vec![PathNode {
                entity_id: from,
                depth: 0,
                via_edge: None,
            }]));
        }

        let graph = self.graph(namespace)?;

        // Forward map: node -> (depth, parent, edge_id that reached this node)
        let mut fwd: HashMap<Uuid, (usize, Option<Uuid>, Option<Uuid>)> = HashMap::new();
        let mut fwd_q: VecDeque<Uuid> = VecDeque::new();
        fwd.insert(from, (0, None, None));
        fwd_q.push_back(from);

        // Backward map: node -> (depth, child, edge_id that reached this node from `to` side)
        let mut bwd: HashMap<Uuid, (usize, Option<Uuid>, Option<Uuid>)> = HashMap::new();
        let mut bwd_q: VecDeque<Uuid> = VecDeque::new();
        bwd.insert(to, (0, None, None));
        bwd_q.push_back(to);

        let mut meeting: Option<(Uuid, usize)> = None;
        let mut current_depth = 0usize;

        while (!fwd_q.is_empty() || !bwd_q.is_empty()) && current_depth <= max_depth {
            // Expand the forward frontier one level.
            let fwd_level = fwd_q.len();
            for _ in 0..fwd_level {
                let Some(node) = fwd_q.pop_front() else { break };
                let fwd_depth = fwd[&node].0;

                let hits = graph
                    .neighbors(
                        node,
                        NeighborQuery {
                            direction: Direction::Out,
                            relations: None,
                            limit: None,
                            min_weight: None,
                        },
                    )
                    .await?;

                for hit in hits {
                    if fwd.contains_key(&hit.node_id) {
                        continue;
                    }
                    let new_depth = fwd_depth + 1;
                    fwd.insert(hit.node_id, (new_depth, Some(node), Some(hit.edge_id)));
                    fwd_q.push_back(hit.node_id);

                    if let Some(&(bwd_depth, _, _)) = bwd.get(&hit.node_id) {
                        let total = new_depth + bwd_depth;
                        if total <= max_depth
                            && meeting.as_ref().is_none_or(|&(_, best)| total < best)
                        {
                            meeting = Some((hit.node_id, total));
                        }
                    }
                }
            }

            if meeting.is_some() {
                break;
            }

            // Expand the backward frontier one level (following incoming edges).
            let bwd_level = bwd_q.len();
            for _ in 0..bwd_level {
                let Some(node) = bwd_q.pop_front() else { break };
                let bwd_depth = bwd[&node].0;

                let hits = graph
                    .neighbors(
                        node,
                        NeighborQuery {
                            direction: Direction::In,
                            relations: None,
                            limit: None,
                            min_weight: None,
                        },
                    )
                    .await?;

                for hit in hits {
                    if bwd.contains_key(&hit.node_id) {
                        continue;
                    }
                    let new_depth = bwd_depth + 1;
                    bwd.insert(hit.node_id, (new_depth, Some(node), Some(hit.edge_id)));
                    bwd_q.push_back(hit.node_id);

                    if let Some(&(fwd_depth, _, _)) = fwd.get(&hit.node_id) {
                        let total = fwd_depth + new_depth;
                        if total <= max_depth
                            && meeting.as_ref().is_none_or(|&(_, best)| total < best)
                        {
                            meeting = Some((hit.node_id, total));
                        }
                    }
                }
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

        // Reconstruct path: walk fwd map back from mid to `from`, then walk bwd map forward to `to`.
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
            // Walk toward `to` using the backward map's child pointers.
            while let Some(&(_, Some(child), edge_id)) = bwd.get(&cur) {
                bwd_chain.push((child, edge_id));
                cur = child;
            }
        }

        // Build PathNode slice — fetch edges lazily.
        let mut path: Vec<PathNode> = Vec::new();
        for (i, (node_id, edge_id)) in fwd_chain.iter().enumerate() {
            let via_edge = if i == 0 {
                None // start node
            } else if let Some(eid) = edge_id {
                graph.get_edge(LinkId::from(*eid)).await?.or(None)
            } else {
                None
            };
            path.push(PathNode {
                entity_id: *node_id,
                depth: i,
                via_edge,
            });
        }

        let base = path.len();
        for (i, (node_id, edge_id)) in bwd_chain.iter().enumerate() {
            let via_edge = if let Some(eid) = edge_id {
                graph.get_edge(LinkId::from(*eid)).await?.or(None)
            } else {
                None
            };
            path.push(PathNode {
                entity_id: *node_id,
                depth: base + i,
                via_edge,
            });
        }

        Ok(Some(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::KhiveRuntime;
    use khive_storage::EdgeRelation;

    async fn rt() -> KhiveRuntime {
        KhiveRuntime::memory().expect("memory runtime")
    }

    #[tokio::test]
    async fn bfs_max_depth_zero_returns_only_root() {
        let rt = rt().await;
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        rt.link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();

        let opts = TraversalOptions {
            max_depth: 0,
            ..Default::default()
        };
        let nodes = rt.bfs_traverse(None, a.id, opts).await.unwrap();

        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].entity_id, a.id);
        assert_eq!(nodes[0].depth, 0);
        assert!(nodes[0].via_edge.is_none());
    }

    #[tokio::test]
    async fn bfs_depth_one_returns_root_and_neighbors() {
        let rt = rt().await;
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        let c = rt
            .create_entity(None, "concept", "C", None, None, vec![])
            .await
            .unwrap();
        rt.link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        rt.link(None, a.id, c.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        // Add a node two hops away — it must NOT appear.
        let d = rt
            .create_entity(None, "concept", "D", None, None, vec![])
            .await
            .unwrap();
        rt.link(None, b.id, d.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();

        let opts = TraversalOptions {
            max_depth: 1,
            ..Default::default()
        };
        let nodes = rt.bfs_traverse(None, a.id, opts).await.unwrap();

        let ids: HashSet<Uuid> = nodes.iter().map(|n| n.entity_id).collect();
        assert!(ids.contains(&a.id));
        assert!(ids.contains(&b.id));
        assert!(ids.contains(&c.id));
        assert!(!ids.contains(&d.id));
        // Every non-root node should be at depth 1.
        for node in &nodes {
            if node.entity_id != a.id {
                assert_eq!(node.depth, 1);
            }
        }
    }

    #[tokio::test]
    async fn bfs_direction_out_only() {
        let rt = rt().await;
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        // Edge goes B -> A; traversing Out from A should find nothing.
        rt.link(None, b.id, a.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();

        let opts = TraversalOptions {
            max_depth: 2,
            direction: Direction::Out,
            ..Default::default()
        };
        let nodes = rt.bfs_traverse(None, a.id, opts).await.unwrap();
        assert_eq!(
            nodes.len(),
            1,
            "only root should be returned when traversing Out with no outgoing edges"
        );
    }

    #[tokio::test]
    async fn bfs_direction_in_only() {
        let rt = rt().await;
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        // Edge goes B -> A; traversing In from A should find B.
        rt.link(None, b.id, a.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();

        let opts = TraversalOptions {
            max_depth: 2,
            direction: Direction::In,
            ..Default::default()
        };
        let nodes = rt.bfs_traverse(None, a.id, opts).await.unwrap();
        let ids: HashSet<Uuid> = nodes.iter().map(|n| n.entity_id).collect();
        assert!(
            ids.contains(&b.id),
            "B should be reachable via incoming edge"
        );
    }

    #[tokio::test]
    async fn bfs_relation_filter() {
        let rt = rt().await;
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        let c = rt
            .create_entity(None, "concept", "C", None, None, vec![])
            .await
            .unwrap();
        rt.link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        rt.link(None, a.id, c.id, EdgeRelation::DependsOn, 1.0)
            .await
            .unwrap();

        let opts = TraversalOptions {
            max_depth: 2,
            relations: Some(vec![EdgeRelation::Extends]),
            ..Default::default()
        };
        let nodes = rt.bfs_traverse(None, a.id, opts).await.unwrap();
        let ids: HashSet<Uuid> = nodes.iter().map(|n| n.entity_id).collect();
        assert!(ids.contains(&b.id), "B reachable via 'extends'");
        assert!(
            !ids.contains(&c.id),
            "C not reachable when filtering to 'extends'"
        );
    }

    #[tokio::test]
    async fn shortest_path_connected_nodes() {
        let rt = rt().await;
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        let c = rt
            .create_entity(None, "concept", "C", None, None, vec![])
            .await
            .unwrap();
        rt.link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        rt.link(None, b.id, c.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();

        let path = rt.shortest_path(None, a.id, c.id, 10).await.unwrap();
        let path = path.expect("path should exist");
        assert_eq!(path.len(), 3, "A -> B -> C = 3 nodes");
        assert_eq!(path[0].entity_id, a.id);
        assert_eq!(path[2].entity_id, c.id);
    }

    #[tokio::test]
    async fn shortest_path_unreachable_returns_none() {
        let rt = rt().await;
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        // No edges between them.

        let path = rt.shortest_path(None, a.id, b.id, 5).await.unwrap();
        assert!(path.is_none());
    }

    #[tokio::test]
    async fn shortest_path_same_node() {
        let rt = rt().await;
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();

        let path = rt.shortest_path(None, a.id, a.id, 5).await.unwrap();
        let path = path.expect("trivial path should always exist");
        assert_eq!(path.len(), 1);
        assert_eq!(path[0].entity_id, a.id);
        assert!(path[0].via_edge.is_none());
    }

    #[tokio::test]
    async fn shortest_path_max_depth_zero_adjacent() {
        let rt = rt().await;
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        rt.link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();

        // max_depth=0 means only the trivial from==to case succeeds.
        let path = rt.shortest_path(None, a.id, b.id, 0).await.unwrap();
        assert!(
            path.is_none(),
            "1-hop path should not be returned at max_depth=0"
        );
    }

    #[tokio::test]
    async fn shortest_path_max_depth_one_two_hop_chain() {
        let rt = rt().await;
        let a = rt
            .create_entity(None, "concept", "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(None, "concept", "B", None, None, vec![])
            .await
            .unwrap();
        let c = rt
            .create_entity(None, "concept", "C", None, None, vec![])
            .await
            .unwrap();
        rt.link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();
        rt.link(None, b.id, c.id, EdgeRelation::Extends, 1.0)
            .await
            .unwrap();

        // max_depth=1 should find A->B but not A->B->C.
        let one_hop = rt.shortest_path(None, a.id, b.id, 1).await.unwrap();
        assert!(
            one_hop.is_some(),
            "1-hop path should be found at max_depth=1"
        );

        let two_hop = rt.shortest_path(None, a.id, c.id, 1).await.unwrap();
        assert!(
            two_hop.is_none(),
            "2-hop path should not be returned at max_depth=1"
        );
    }
}
