use super::*;

fn make_ref(id: u128, kind: &str) -> AnchorRef {
    AnchorRef {
        id: Uuid::from_u128(id),
        kind: kind.to_string(),
        stable_id: None,
    }
}

#[test]
fn test_anchor_ref_fields() {
    let r = AnchorRef {
        id: Uuid::new_v4(),
        kind: "paper".into(),
        stable_id: Some("doi:10.1234/x".into()),
    };
    assert_eq!(r.kind, "paper");
    assert!(r.stable_id.is_some());
}

#[test]
fn test_anchor_graph_add_and_find() {
    let mut graph = AnchorGraph::new();
    let a = make_ref(1, "record");
    let b = make_ref(2, "source");
    graph.add_node(a.clone());
    graph.add_node(b.clone());
    graph.add_edge(a.id, b.id, "derives_from");

    assert!(graph.find_node(a.id).is_some());
    assert!(graph.find_node(Uuid::nil()).is_none());
}

#[test]
fn test_bfs_anchor_trace_not_found() {
    let graph = AnchorGraph::new();
    let unknown = make_ref(99, "unknown");
    let err = BfsAnchor.trace(&graph, &unknown, 5).unwrap_err();
    assert!(matches!(err, FoldError::AnchorNotFound(_)));
}

#[test]
fn test_bfs_anchor_trace_chain() {
    let mut graph = AnchorGraph::new();
    let a = make_ref(1, "record");
    let b = make_ref(2, "source");
    let c = make_ref(3, "paper");
    graph.add_node(a.clone());
    graph.add_node(b.clone());
    graph.add_node(c.clone());
    graph.add_edge(a.id, b.id, "derives_from");
    graph.add_edge(b.id, c.id, "uses");

    let chain = BfsAnchor.trace(&graph, &a, 5).unwrap();
    assert_eq!(chain.len(), 2);
    assert!(chain.iter().any(|r| r.id == b.id));
    assert!(chain.iter().any(|r| r.id == c.id));
}

#[test]
fn test_bfs_anchor_trace_max_depth() {
    let mut graph = AnchorGraph::new();
    let nodes: Vec<AnchorRef> = (1..=5).map(|i| make_ref(i, "node")).collect();
    for n in &nodes {
        graph.add_node(n.clone());
    }
    for i in 0..4 {
        graph.add_edge(nodes[i].id, nodes[i + 1].id, "next");
    }

    // With max_depth=1, only one hop from start
    let chain = BfsAnchor.trace(&graph, &nodes[0], 1).unwrap();
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].id, nodes[1].id);
}

#[test]
fn test_bfs_anchor_credit_not_found() {
    let graph = AnchorGraph::new();
    let unknown = make_ref(99, "unknown");
    let err = BfsAnchor.credit(&graph, &unknown, 5).unwrap_err();
    assert!(matches!(err, FoldError::AnchorNotFound(_)));
}

#[test]
fn test_bfs_anchor_credit_basic() {
    let mut graph = AnchorGraph::new();
    let source = make_ref(1, "paper");
    let intermediate = make_ref(2, "record");
    let outcome = make_ref(3, "composition");
    graph.add_node(source.clone());
    graph.add_node(intermediate.clone());
    graph.add_node(outcome.clone());
    // edges point forward: source → intermediate → outcome
    graph.add_edge(source.id, intermediate.id, "uses");
    graph.add_edge(intermediate.id, outcome.id, "derives_from");

    let credits = BfsAnchor.credit(&graph, &outcome, 5).unwrap();
    assert!(!credits.is_empty());
    // intermediate should be credited with weight > 0
    let inter_credit = credits.iter().find(|(r, _)| r.id == intermediate.id);
    assert!(inter_credit.is_some());
    assert!(inter_credit.unwrap().1 > 0.0f64);
}

#[test]
fn credit_weights_are_f64() {
    let mut graph = AnchorGraph::new();
    let source = make_ref(10, "source");
    let outcome = make_ref(11, "outcome");
    graph.add_node(source.clone());
    graph.add_node(outcome.clone());
    graph.add_edge(source.id, outcome.id, "causes");

    let credits: Vec<(AnchorRef, f64)> = BfsAnchor.credit(&graph, &outcome, 2).unwrap();
    assert!(!credits.is_empty());
    let w: f64 = credits[0].1;
    assert!(w > 0.0f64 && w <= 1.0f64);
}

#[test]
fn test_bfs_anchor_duplicate_nodes_keep_first_payload() {
    let root = make_ref(1, "root");
    let first = AnchorRef {
        id: Uuid::from_u128(2),
        kind: "first".into(),
        stable_id: Some("first-payload".into()),
    };
    let duplicate = AnchorRef {
        id: first.id,
        kind: "later".into(),
        stable_id: Some("later-payload".into()),
    };
    let outcome = make_ref(3, "outcome");
    let mut graph = AnchorGraph::new();
    graph.nodes = vec![root.clone(), first.clone(), duplicate, outcome.clone()];
    graph.add_edge(root.id, first.id, "uses");
    graph.add_edge(first.id, outcome.id, "derives_from");

    assert_eq!(
        BfsAnchor.trace(&graph, &root, 2).unwrap(),
        vec![first.clone(), outcome.clone()]
    );
    assert_eq!(
        BfsAnchor.credit(&graph, &outcome, 2).unwrap(),
        vec![(first, 0.5), (root, 0.25)]
    );
}

#[test]
fn test_bfs_anchor_keeps_edge_order_cycles_and_first_visit() {
    let a = make_ref(10, "root");
    let b = make_ref(20, "second");
    let c = make_ref(30, "first");
    let d = make_ref(40, "join");
    let e = make_ref(50, "outcome");
    let mut graph = AnchorGraph::new();
    graph.nodes = vec![a.clone(), b.clone(), c.clone(), d.clone(), e.clone()];
    for (from, to) in [
        (a.id, c.id),
        (a.id, b.id),
        (a.id, c.id),
        (a.id, a.id),
        (c.id, d.id),
        (b.id, d.id),
        (d.id, a.id),
        (d.id, e.id),
        (c.id, e.id),
    ] {
        graph.add_edge(from, to, "uses");
    }

    assert_eq!(
        BfsAnchor.trace(&graph, &a, 1).unwrap(),
        vec![c.clone(), b.clone()]
    );
    assert_eq!(
        BfsAnchor.trace(&graph, &a, usize::MAX).unwrap(),
        vec![c.clone(), b.clone(), d.clone(), e.clone()]
    );
    assert_eq!(
        BfsAnchor.credit(&graph, &e, 1).unwrap(),
        vec![(d.clone(), 0.5), (c.clone(), 0.5)]
    );
    assert_eq!(
        BfsAnchor.credit(&graph, &e, usize::MAX).unwrap(),
        vec![(d, 0.5), (c, 0.5), (b, 0.25), (a, 0.25)]
    );
}

#[test]
fn test_bfs_anchor_preserves_missing_node_asymmetry_and_zero_depth() {
    let a = make_ref(100, "source");
    let missing = make_ref(200, "missing");
    let outcome = make_ref(300, "outcome");
    let mut graph = AnchorGraph::new();
    graph.nodes = vec![a.clone(), outcome.clone()];
    graph.add_edge(a.id, missing.id, "uses");
    graph.add_edge(missing.id, outcome.id, "derives_from");

    // Trace stops at a missing node, but credit crosses it and counts both hops.
    assert!(BfsAnchor.trace(&graph, &a, 2).unwrap().is_empty());
    assert!(BfsAnchor.credit(&graph, &outcome, 1).unwrap().is_empty());
    assert_eq!(
        BfsAnchor.credit(&graph, &outcome, 2).unwrap(),
        vec![(a.clone(), 0.25)]
    );
    assert!(BfsAnchor.trace(&graph, &a, 0).unwrap().is_empty());
    assert!(BfsAnchor.credit(&graph, &outcome, 0).unwrap().is_empty());
    for depth in [0, 2] {
        let trace_error = BfsAnchor.trace(&graph, &missing, depth).unwrap_err();
        let credit_error = BfsAnchor.credit(&graph, &missing, depth).unwrap_err();
        assert!(
            matches!(trace_error, FoldError::AnchorNotFound(id) if id == missing.id.to_string())
        );
        assert!(
            matches!(credit_error, FoldError::AnchorNotFound(id) if id == missing.id.to_string())
        );
    }
}

#[test]
fn test_bfs_anchor_rebuilds_after_public_vector_mutation() {
    let root = make_ref(1, "root");
    let old_child = make_ref(2, "old-child");
    let leaf = make_ref(3, "leaf");
    let mut graph = AnchorGraph::new();
    graph.nodes = vec![root.clone(), old_child.clone()];
    graph.add_edge(root.id, old_child.id, "uses");
    assert_eq!(
        BfsAnchor.trace(&graph, &root, 2).unwrap(),
        vec![old_child.clone()]
    );
    assert_eq!(
        BfsAnchor.credit(&graph, &old_child, 2).unwrap(),
        vec![(root.clone(), 0.5)]
    );

    let new_child = AnchorRef {
        id: old_child.id,
        kind: "new-first-child".into(),
        stable_id: Some("mutated".into()),
    };
    graph.nodes.insert(0, new_child.clone());
    graph.nodes.push(leaf.clone());
    graph.edges = vec![
        (root.id, leaf.id, "new-first-edge".into()),
        (root.id, new_child.id, "new-second-edge".into()),
        (leaf.id, new_child.id, "uses".into()),
    ];
    assert_eq!(
        BfsAnchor.trace(&graph, &root, 2).unwrap(),
        vec![leaf.clone(), new_child.clone()]
    );
    assert_eq!(
        BfsAnchor.credit(&graph, &new_child, 2).unwrap(),
        vec![(root.clone(), 0.5), (leaf.clone(), 0.5)]
    );

    graph.nodes.retain(|node| node.id != leaf.id);
    assert_eq!(
        BfsAnchor.trace(&graph, &root, 2).unwrap(),
        vec![new_child.clone()]
    );
    assert_eq!(
        BfsAnchor.credit(&graph, &new_child, 2).unwrap(),
        vec![(root, 0.5)]
    );
}

#[test]
fn test_bfs_anchor_credit_keeps_per_hop_subnormal_rounding() {
    const HOPS: usize = 1075;
    let mut graph = AnchorGraph::new();
    graph.nodes = (0..=HOPS)
        .map(|index| make_ref(1000 + index as u128, "node"))
        .collect();
    for index in 0..HOPS {
        graph.add_edge(graph.nodes[index].id, graph.nodes[index + 1].id, "uses");
    }
    let credits = BfsAnchor.credit(&graph, &graph.nodes[HOPS], HOPS).unwrap();
    let actual_nodes: Vec<_> = credits.iter().map(|(node, _)| node).collect();
    let expected_nodes: Vec<_> = graph.nodes[..HOPS].iter().rev().collect();
    assert_eq!(actual_nodes, expected_nodes);

    // Exact binary64 powers at normal, subnormal and round-to-zero boundaries.
    for (distance, bits) in [
        (1, 0.5_f64.to_bits()),
        (1022, 0x0010_0000_0000_0000),
        (1023, 0x0008_0000_0000_0000),
        (1074, 1),
        (1075, 0),
    ] {
        assert_eq!(
            credits[distance - 1].1.to_bits(),
            bits,
            "distance {distance}"
        );
    }
}
