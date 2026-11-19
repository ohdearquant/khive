#[tokio::test]
async fn test_traverse_depth_2() {
    let store = setup_memory_store();

    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    let d = Uuid::new_v4();

    store
        .upsert_edge(make_edge(a, b, EdgeRelation::Extends, 1.0))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(b, c, EdgeRelation::Extends, 2.0))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(c, d, EdgeRelation::Extends, 3.0))
        .await
        .unwrap();

    let request = TraversalRequest {
        roots: vec![a],
        options: TraversalOptions::new(2).with_direction(Direction::Out),
        include_roots: true,
        include_properties: false,
        execution_budget: Default::default(),
    };

    let paths = store.traverse(request).await.unwrap();
    assert_eq!(paths.len(), 1);

    let path = &paths[0];
    let node_ids: Vec<Uuid> = path.nodes.iter().map(|n| n.node_id).collect();
    assert!(node_ids.contains(&a));
    assert!(node_ids.contains(&b));
    assert!(node_ids.contains(&c));
    assert!(!node_ids.contains(&d));
}

/// Diamond graph: A→B, A→C, B→D, C→D.
/// D is reachable via two paths at depth 2.  After the fix it must appear
/// exactly once in the result.
#[tokio::test]
async fn test_traverse_dedups_multipath_node() {
    let store = setup_memory_store();

    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    let d = Uuid::new_v4();

    store
        .upsert_edge(make_edge(a, b, EdgeRelation::Extends, 1.0))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(a, c, EdgeRelation::Extends, 1.0))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(b, d, EdgeRelation::Extends, 1.0))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(c, d, EdgeRelation::Extends, 1.0))
        .await
        .unwrap();

    let request = TraversalRequest {
        roots: vec![a],
        options: TraversalOptions::new(3).with_direction(Direction::Out),
        include_roots: false,
        include_properties: false,
        execution_budget: Default::default(),
    };

    let paths = store.traverse(request).await.unwrap();
    assert_eq!(paths.len(), 1);
    let nodes = &paths[0].nodes;

    // D must appear exactly once despite being reachable via both B and C.
    let d_count = nodes.iter().filter(|n| n.node_id == d).count();
    assert_eq!(d_count, 1, "D must appear exactly once (dedup multi-path)");

    // B and C must each appear once as well.
    assert_eq!(nodes.iter().filter(|n| n.node_id == b).count(), 1);
    assert_eq!(nodes.iter().filter(|n| n.node_id == c).count(), 1);
}

/// First-visit BFS keeps the shallowest occurrence of a node, and the
/// `via_edge` recorded for it is the one from that first-visited path.
///
/// Graph: A→B (depth 1), A→C (depth 1), B→D (depth 2), C→D (depth 2).
/// D appears at depth 2 via B or C. Whichever same-depth frontier path the
/// backend enumerates first is the keeper. The test
/// asserts that D has exactly one entry with a non-None `via_edge` — we
/// do NOT assert *which* edge wins because SQLite row order within the
/// same depth level is non-deterministic, but we DO assert stability:
/// running twice gives the same count.
#[tokio::test]
async fn test_traverse_preserves_first_path_metadata() {
    let store = setup_memory_store();

    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    let d = Uuid::new_v4();

    store
        .upsert_edge(make_edge(a, b, EdgeRelation::Extends, 1.0))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(a, c, EdgeRelation::Extends, 1.0))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(b, d, EdgeRelation::Extends, 1.0))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(c, d, EdgeRelation::Extends, 1.0))
        .await
        .unwrap();

    let make_request = || TraversalRequest {
        roots: vec![a],
        options: TraversalOptions::new(3).with_direction(Direction::Out),
        include_roots: false,
        include_properties: false,
        execution_budget: Default::default(),
    };

    let paths1 = store.traverse(make_request()).await.unwrap();
    let paths2 = store.traverse(make_request()).await.unwrap();

    // Both runs must return the same total node count (dedup is stable).
    let count1 = paths1[0].nodes.len();
    let count2 = paths2[0].nodes.len();
    assert_eq!(
        count1, count2,
        "traverse result count must be stable across calls"
    );

    // D must appear exactly once and carry a via_edge (it was not a root).
    let d_nodes: Vec<_> = paths1[0].nodes.iter().filter(|n| n.node_id == d).collect();
    assert_eq!(d_nodes.len(), 1, "D deduped to one entry");
    assert!(
        d_nodes[0].via_edge.is_some(),
        "kept entry must have a via_edge"
    );
    assert_eq!(d_nodes[0].depth, 2, "D lives at depth 2");
}

/// Multi-root traversal: two independent chains A→B→C and D→E→F.
/// Each root must produce its own GraphPath with the correct node set.
#[tokio::test]
async fn test_traverse_multi_root_independent_chains() {
    let store = setup_memory_store();

    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    let d = Uuid::new_v4();
    let e = Uuid::new_v4();
    let f = Uuid::new_v4();

    for (src, tgt) in [(a, b), (b, c), (d, e), (e, f)] {
        store
            .upsert_edge(make_edge(src, tgt, EdgeRelation::Extends, 1.0))
            .await
            .unwrap();
    }

    let request = TraversalRequest {
        roots: vec![a, d],
        options: TraversalOptions::new(2).with_direction(Direction::Out),
        include_roots: true,
        include_properties: false,
        execution_budget: Default::default(),
    };

    let paths = store.traverse(request).await.unwrap();
    assert_eq!(paths.len(), 2, "one GraphPath per root");

    // Locate paths by root_id.
    let path_a = paths
        .iter()
        .find(|p| p.root_id == a)
        .expect("path for root A");
    let path_d = paths
        .iter()
        .find(|p| p.root_id == d)
        .expect("path for root D");

    let ids_a: HashSet<Uuid> = path_a.nodes.iter().map(|n| n.node_id).collect();
    assert!(ids_a.contains(&a), "root A in its own path");
    assert!(ids_a.contains(&b), "depth-1 B in A's path");
    assert!(ids_a.contains(&c), "depth-2 C in A's path");
    assert!(!ids_a.contains(&d), "root D must not appear in A's path");

    let ids_d: HashSet<Uuid> = path_d.nodes.iter().map(|n| n.node_id).collect();
    assert!(ids_d.contains(&d), "root D in its own path");
    assert!(ids_d.contains(&e), "depth-1 E in D's path");
    assert!(ids_d.contains(&f), "depth-2 F in D's path");
    assert!(!ids_d.contains(&a), "root A must not appear in D's path");
}

/// Multi-root with a shared neighbor: A→C and B→C.  C must appear in BOTH
/// A's path and B's path (per-root isolation, not global dedup).
#[tokio::test]
async fn test_traverse_multi_root_shared_neighbor_appears_in_both() {
    let store = setup_memory_store();

    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4(); // shared neighbor

    for (src, tgt) in [(a, c), (b, c)] {
        store
            .upsert_edge(make_edge(src, tgt, EdgeRelation::Extends, 1.0))
            .await
            .unwrap();
    }

    let request = TraversalRequest {
        roots: vec![a, b],
        options: TraversalOptions::new(1).with_direction(Direction::Out),
        include_roots: false,
        include_properties: false,
        execution_budget: Default::default(),
    };

    let paths = store.traverse(request).await.unwrap();
    assert_eq!(paths.len(), 2, "one GraphPath per root");

    for path in &paths {
        let node_ids: HashSet<Uuid> = path.nodes.iter().map(|n| n.node_id).collect();
        assert!(
            node_ids.contains(&c),
            "shared node C must appear in each root's path; root={:?}",
            path.root_id
        );
    }
}
