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
mod tests {
    use super::*;
    use crate::runtime::{KhiveRuntime, NamespaceToken};
    use khive_storage::EdgeRelation;

    async fn rt() -> KhiveRuntime {
        KhiveRuntime::memory().expect("memory runtime")
    }

    #[tokio::test]
    async fn bfs_max_depth_zero_returns_only_root() {
        let rt = rt().await;
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        rt.link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();

        let opts = TraversalOptions {
            max_depth: 0,
            ..Default::default()
        };
        let nodes = rt.bfs_traverse(&tok, a.id, opts).await.unwrap();

        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].entity_id, a.id);
        assert_eq!(nodes[0].depth, 0);
        assert!(nodes[0].via_edge.is_none());
    }

    #[tokio::test]
    async fn bfs_depth_one_returns_root_and_neighbors() {
        let rt = rt().await;
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let c = rt
            .create_entity(&tok, "concept", None, "C", None, None, vec![])
            .await
            .unwrap();
        rt.link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, a.id, c.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        // Add a node two hops away — it must NOT appear.
        let d = rt
            .create_entity(&tok, "concept", None, "D", None, None, vec![])
            .await
            .unwrap();
        rt.link(&tok, b.id, d.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();

        let opts = TraversalOptions {
            max_depth: 1,
            ..Default::default()
        };
        let nodes = rt.bfs_traverse(&tok, a.id, opts).await.unwrap();

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
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        // Edge goes B -> A; traversing Out from A should find nothing.
        rt.link(&tok, b.id, a.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();

        let opts = TraversalOptions {
            max_depth: 2,
            direction: Direction::Out,
            ..Default::default()
        };
        let nodes = rt.bfs_traverse(&tok, a.id, opts).await.unwrap();
        assert_eq!(
            nodes.len(),
            1,
            "only root should be returned when traversing Out with no outgoing edges"
        );
    }

    #[tokio::test]
    async fn bfs_direction_in_only() {
        let rt = rt().await;
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        // Edge goes B -> A; traversing In from A should find B.
        rt.link(&tok, b.id, a.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();

        let opts = TraversalOptions {
            max_depth: 2,
            direction: Direction::In,
            ..Default::default()
        };
        let nodes = rt.bfs_traverse(&tok, a.id, opts).await.unwrap();
        let ids: HashSet<Uuid> = nodes.iter().map(|n| n.entity_id).collect();
        assert!(
            ids.contains(&b.id),
            "B should be reachable via incoming edge"
        );
    }

    #[tokio::test]
    async fn bfs_relation_filter() {
        let rt = rt().await;
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let c = rt
            .create_entity(&tok, "concept", None, "C", None, None, vec![])
            .await
            .unwrap();
        rt.link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, a.id, c.id, EdgeRelation::Enables, 1.0, None)
            .await
            .unwrap();

        let opts = TraversalOptions {
            max_depth: 2,
            relations: Some(vec![EdgeRelation::Extends]),
            ..Default::default()
        };
        let nodes = rt.bfs_traverse(&tok, a.id, opts).await.unwrap();
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
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let c = rt
            .create_entity(&tok, "concept", None, "C", None, None, vec![])
            .await
            .unwrap();
        rt.link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, b.id, c.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();

        let path = rt.shortest_path(&tok, a.id, c.id, 10).await.unwrap();
        let path = path.expect("path should exist");
        assert_eq!(path.len(), 3, "A -> B -> C = 3 nodes");
        assert_eq!(path[0].entity_id, a.id);
        assert_eq!(path[2].entity_id, c.id);
    }

    #[tokio::test]
    async fn shortest_path_unreachable_returns_none() {
        let rt = rt().await;
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();

        let path = rt.shortest_path(&tok, a.id, b.id, 5).await.unwrap();
        assert!(path.is_none());
    }

    #[tokio::test]
    async fn shortest_path_same_node() {
        let rt = rt().await;
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();

        let path = rt.shortest_path(&tok, a.id, a.id, 5).await.unwrap();
        let path = path.expect("trivial path should always exist");
        assert_eq!(path.len(), 1);
        assert_eq!(path[0].entity_id, a.id);
        assert!(path[0].via_edge.is_none());
    }

    #[tokio::test]
    async fn shortest_path_max_depth_zero_adjacent() {
        let rt = rt().await;
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        rt.link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();

        // max_depth=0 means only the trivial from==to case succeeds.
        let path = rt.shortest_path(&tok, a.id, b.id, 0).await.unwrap();
        assert!(
            path.is_none(),
            "1-hop path should not be returned at max_depth=0"
        );
    }

    #[tokio::test]
    async fn shortest_path_max_depth_one_two_hop_chain() {
        let rt = rt().await;
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let c = rt
            .create_entity(&tok, "concept", None, "C", None, None, vec![])
            .await
            .unwrap();
        rt.link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, b.id, c.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();

        // max_depth=1 should find A->B but not A->B->C.
        let one_hop = rt.shortest_path(&tok, a.id, b.id, 1).await.unwrap();
        assert!(
            one_hop.is_some(),
            "1-hop path should be found at max_depth=1"
        );

        let two_hop = rt.shortest_path(&tok, a.id, c.id, 1).await.unwrap();
        assert!(
            two_hop.is_none(),
            "2-hop path should not be returned at max_depth=1"
        );
    }

    // Proves batched BFS issues O(max_depth) round-trips, not O(nodes+edges), over a
    // 15-node/14-edge depth-3 binary tree (root -> n1,n2 -> n3..n6 -> l1..l8).
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn bfs_query_count_is_o_depth_not_o_nodes() {
        use crate::runtime::KhiveRuntime;

        let rt = KhiveRuntime::memory().expect("memory runtime");
        let tok = NamespaceToken::local();

        let root = rt
            .create_entity(&tok, "concept", None, "root", None, None, vec![])
            .await
            .unwrap();
        let n1 = rt
            .create_entity(&tok, "concept", None, "n1", None, None, vec![])
            .await
            .unwrap();
        let n2 = rt
            .create_entity(&tok, "concept", None, "n2", None, None, vec![])
            .await
            .unwrap();
        let n3 = rt
            .create_entity(&tok, "concept", None, "n3", None, None, vec![])
            .await
            .unwrap();
        let n4 = rt
            .create_entity(&tok, "concept", None, "n4", None, None, vec![])
            .await
            .unwrap();
        let n5 = rt
            .create_entity(&tok, "concept", None, "n5", None, None, vec![])
            .await
            .unwrap();
        let n6 = rt
            .create_entity(&tok, "concept", None, "n6", None, None, vec![])
            .await
            .unwrap();
        let leaves: Vec<_> = ["l1", "l2", "l3", "l4", "l5", "l6", "l7", "l8"]
            .iter()
            .map(|n| n.to_string())
            .collect();
        let mut leaf_ids = Vec::new();
        for name in &leaves {
            let e = rt
                .create_entity(&tok, "concept", None, name.as_str(), None, None, vec![])
                .await
                .unwrap();
            leaf_ids.push(e);
        }

        rt.link(&tok, root.id, n1.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, root.id, n2.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, n1.id, n3.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, n1.id, n4.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, n2.id, n5.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, n2.id, n6.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let depth2 = [n3.id, n4.id, n5.id, n6.id];
        for (i, parent) in depth2.iter().enumerate() {
            rt.link(
                &tok,
                *parent,
                leaf_ids[i * 2].id,
                EdgeRelation::Extends,
                1.0,
                None,
            )
            .await
            .unwrap();
            rt.link(
                &tok,
                *parent,
                leaf_ids[i * 2 + 1].id,
                EdgeRelation::Extends,
                1.0,
                None,
            )
            .await
            .unwrap();
        }

        let opts = TraversalOptions {
            max_depth: 3,
            ..Default::default()
        };
        let nodes = rt.bfs_traverse(&tok, root.id, opts).await.unwrap();
        assert_eq!(nodes.len(), 15, "all 15 nodes in the tree must be returned");

        assert_eq!(nodes[0].depth, 0);
        let depth1_count = nodes.iter().filter(|n| n.depth == 1).count();
        let depth2_count = nodes.iter().filter(|n| n.depth == 2).count();
        let depth3_count = nodes.iter().filter(|n| n.depth == 3).count();
        assert_eq!(depth1_count, 2);
        assert_eq!(depth2_count, 4);
        assert_eq!(depth3_count, 8);

        for node in nodes.iter().skip(1) {
            assert!(
                node.via_edge.is_some(),
                "non-root node at depth {} must have via_edge",
                node.depth
            );
        }

        // No call-counting hook exists on bfs_traverse itself, so the same GraphStore
        // is driven manually, level by level, to count round-trips directly.
        let graph = rt.graph(&tok).expect("graph store");

        let get_edge_counter = Arc::new(AtomicUsize::new(0));
        let get_edges_counter = Arc::new(AtomicUsize::new(0));
        let neighbors_counter = Arc::new(AtomicUsize::new(0));
        let batch_neighbors_counter = Arc::new(AtomicUsize::new(0));

        let mut sim_visited: HashSet<Uuid> = HashSet::new();
        let mut sim_results: Vec<Uuid> = Vec::new();
        let mut sim_frontier: Vec<Uuid> = vec![root.id];
        sim_visited.insert(root.id);
        sim_results.push(root.id);
        let mut sim_depth = 0usize;

        while !sim_frontier.is_empty() && sim_depth < 3 {
            let query = NeighborQuery {
                direction: Direction::Out,
                relations: None,
                limit: None,
                min_weight: None,
            };
            batch_neighbors_counter.fetch_add(1, Ordering::Relaxed);
            let all_hits = graph.batch_neighbors(&sim_frontier, query).await.unwrap();

            let mut level_new: Vec<(Uuid, Uuid)> = Vec::new();
            for (_src, hit) in &all_hits {
                if sim_visited.insert(hit.node_id) {
                    level_new.push((hit.node_id, hit.edge_id));
                }
            }
            if level_new.is_empty() {
                break;
            }

            let edge_ids: Vec<LinkId> = level_new
                .iter()
                .map(|(_, eid)| LinkId::from(*eid))
                .collect();
            get_edges_counter.fetch_add(1, Ordering::Relaxed);
            let _edges = graph.get_edges(&edge_ids).await.unwrap();

            sim_frontier.clear();
            for (node_id, _) in &level_new {
                sim_results.push(*node_id);
                sim_frontier.push(*node_id);
            }
            sim_depth += 1;
        }

        assert_eq!(sim_results.len(), 15, "simulation must find all 15 nodes");

        let bn_calls = batch_neighbors_counter.load(Ordering::Relaxed);
        let ge_calls = get_edges_counter.load(Ordering::Relaxed);
        let n_calls = neighbors_counter.load(Ordering::Relaxed);
        let ges_calls = get_edge_counter.load(Ordering::Relaxed);

        assert_eq!(
            bn_calls, 3,
            "batch_neighbors must be called once per BFS level (3 levels)"
        );
        assert_eq!(
            ge_calls, 3,
            "get_edges must be called once per BFS level (3 levels)"
        );
        assert_eq!(n_calls, 0, "old single-node neighbors() must NOT be called");
        assert_eq!(
            ges_calls, 0,
            "old single-edge get_edge() must NOT be called"
        );
    }
}
