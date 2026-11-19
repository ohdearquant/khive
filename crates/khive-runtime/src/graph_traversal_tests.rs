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
