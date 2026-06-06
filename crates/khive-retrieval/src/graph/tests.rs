//! Unit tests for graph traversal module.

use super::compat::{test_context, EntityRef, LinkStore, MockLinkStore};

use crate::graph::types::{
    Direction, PathNode, TraversalOptions, MAX_TRAVERSAL_DEPTH, MAX_TRAVERSAL_RESULTS,
};

#[test]
fn test_traversal_options_default() {
    let opts = TraversalOptions::default();
    assert_eq!(opts.max_depth, 3);
    assert_eq!(opts.direction, Direction::Out);
    assert!(opts.link_types.is_none());
    assert_eq!(opts.limit, Some(MAX_TRAVERSAL_RESULTS));
}

#[test]
fn test_traversal_options_builder() {
    let opts = TraversalOptions::new(5)
        .with_direction(Direction::Both)
        .with_link_types(["contains", "references"])
        .with_limit(100)
        .with_min_weight(0.5);

    assert_eq!(opts.max_depth, 5);
    assert_eq!(opts.direction, Direction::Both);
    assert_eq!(
        opts.link_types,
        Some(vec!["contains".to_string(), "references".to_string()])
    );
    assert_eq!(opts.limit, Some(100));
    assert_eq!(opts.min_weight, Some(0.5));
}

#[test]
fn test_traversal_options_clamping() {
    // Depth clamping
    let opts = TraversalOptions::new(100);
    assert_eq!(opts.max_depth, MAX_TRAVERSAL_DEPTH);

    // Limit clamping
    let opts = TraversalOptions::new(3).with_limit(100_000);
    assert_eq!(opts.limit, Some(MAX_TRAVERSAL_RESULTS));
}

#[test]
fn test_path_node_start() {
    let entity = EntityRef::External("test".to_string());
    let node = PathNode::start(entity.clone());

    assert_eq!(node.entity_id, entity);
    assert_eq!(node.depth, 0);
    assert!(node.via_link.is_none());
    assert_eq!(node.path_weight, 0.0);
}

#[test]
fn test_direction_default() {
    let dir = Direction::default();
    assert_eq!(dir, Direction::Out);
}

#[test]
fn test_safety_constants() {
    // Verify safety constants are reasonable
    assert_eq!(MAX_TRAVERSAL_DEPTH, 20);
    assert_eq!(MAX_TRAVERSAL_RESULTS, 10_000);
}

#[tokio::test]
async fn shortest_path_includes_target_node() {
    // Graph: A → B → C. Verify path is [A, B, C] — all three nodes including target C.
    let store = MockLinkStore::new();
    let ctx = test_context();

    let a = EntityRef::External("A".to_string());
    let b = EntityRef::External("B".to_string());
    let c = EntityRef::External("C".to_string());

    store
        .link(
            &ctx,
            a.clone(),
            b.clone(),
            "edge",
            None::<serde_json::Value>,
        )
        .await
        .unwrap();
    store
        .link(&ctx, b.clone(), c.clone(), "edge", None)
        .await
        .unwrap();

    let path = super::shortest::find_shortest_path(&store, &ctx, a.clone(), c.clone(), 5)
        .await
        .unwrap()
        .expect("path exists");

    assert_eq!(path.len(), 3, "path should contain 3 nodes: A, B, C");
    assert_eq!(path[0].entity_id, a, "first node is start (A)");
    assert_eq!(path[2].entity_id, c, "last node is target (C)");
}

#[tokio::test]
async fn shortest_path_direct_edge_includes_target() {
    // Graph: A → B (direct). Path should be [A, B], not just [A].
    let store = MockLinkStore::new();
    let ctx = test_context();

    let a = EntityRef::External("X".to_string());
    let b = EntityRef::External("Y".to_string());

    store
        .link(
            &ctx,
            a.clone(),
            b.clone(),
            "edge",
            None::<serde_json::Value>,
        )
        .await
        .unwrap();

    let path = super::shortest::find_shortest_path(&store, &ctx, a.clone(), b.clone(), 5)
        .await
        .unwrap()
        .expect("path exists");

    assert_eq!(path.len(), 2, "path should contain 2 nodes: X, Y");
    assert_eq!(path[0].entity_id, a);
    assert_eq!(path[1].entity_id, b, "target node must be in path");
}

#[tokio::test]
async fn shortest_path_max_depth_zero_returns_none_for_non_adjacent() {
    // With max_depth=0, A → B should return None (1-hop path exceeds budget).
    // Regression guard: the old `current_depth > max_depth` check allowed expansion
    // at depth 0, inserting nodes at depth 1 even when max_depth was 0.
    let store = MockLinkStore::new();
    let ctx = test_context();

    let a = EntityRef::External("P".to_string());
    let b = EntityRef::External("Q".to_string());

    store
        .link(
            &ctx,
            a.clone(),
            b.clone(),
            "edge",
            None::<serde_json::Value>,
        )
        .await
        .unwrap();

    let path = super::shortest::find_shortest_path(&store, &ctx, a.clone(), b.clone(), 0)
        .await
        .unwrap();

    assert!(
        path.is_none(),
        "max_depth=0 must not return a 1-hop path; got: {path:?}"
    );
}

#[tokio::test]
async fn shortest_path_max_depth_respects_exact_limit() {
    // Graph: A → B → C. max_depth=2 should find A→B→C; max_depth=1 should return None.
    let store = MockLinkStore::new();
    let ctx = test_context();

    let a = EntityRef::External("A2".to_string());
    let b = EntityRef::External("B2".to_string());
    let c = EntityRef::External("C2".to_string());

    store
        .link(
            &ctx,
            a.clone(),
            b.clone(),
            "edge",
            None::<serde_json::Value>,
        )
        .await
        .unwrap();
    store
        .link(
            &ctx,
            b.clone(),
            c.clone(),
            "edge",
            None::<serde_json::Value>,
        )
        .await
        .unwrap();

    // max_depth=2: path of length 2 (A→B→C) should be found.
    let path = super::shortest::find_shortest_path(&store, &ctx, a.clone(), c.clone(), 2)
        .await
        .unwrap();
    assert!(path.is_some(), "max_depth=2 should find A→B→C");

    // max_depth=1: path requires 2 hops, must return None.
    let path1 = super::shortest::find_shortest_path(&store, &ctx, a.clone(), c.clone(), 1)
        .await
        .unwrap();
    assert!(path1.is_none(), "max_depth=1 must not find a 2-hop path");
}
