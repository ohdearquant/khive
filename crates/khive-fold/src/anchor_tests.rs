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
