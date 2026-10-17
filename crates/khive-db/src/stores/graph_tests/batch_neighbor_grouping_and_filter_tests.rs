#[tokio::test]
async fn batch_neighbors_keeps_exact_rows_grouped_by_requested_node() {
    let store = setup_memory_store();
    let root_a = Uuid::new_v4();
    let root_b = Uuid::new_v4();
    let root_without_edges = Uuid::new_v4();
    let joined = make_edge(root_a, root_b, EdgeRelation::Extends, 0.9);
    let self_loop = make_edge(root_b, root_b, EdgeRelation::DependsOn, 0.8);
    let joined_id = Uuid::from(joined.id);
    let self_loop_id = Uuid::from(self_loop.id);
    for edge in [joined, self_loop] {
        store.upsert_edge(edge).await.unwrap();
    }

    let sources = [root_a, root_b, root_without_edges];
    let outgoing_hits = store
        .batch_neighbors(
            &sources,
            NeighborQuery {
                direction: Direction::Out,
                relations: None,
                limit: None,
                min_weight: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        neighbor_edge_multiset(&outgoing_hits),
        HashMap::from([((root_a, joined_id), 1), ((root_b, self_loop_id), 1),])
    );

    let incoming_hits = store
        .batch_neighbors(
            &sources,
            NeighborQuery {
                direction: Direction::In,
                relations: None,
                limit: None,
                min_weight: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        neighbor_edge_multiset(&incoming_hits),
        HashMap::from([((root_b, joined_id), 1), ((root_b, self_loop_id), 1),])
    );

    let both_hits = store
        .batch_neighbors(
            &sources,
            NeighborQuery {
                direction: Direction::Both,
                relations: None,
                limit: None,
                min_weight: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        neighbor_edge_multiset(&both_hits),
        HashMap::from([
            ((root_a, joined_id), 1),
            ((root_b, joined_id), 1),
            ((root_b, self_loop_id), 2),
        ])
    );
    assert!(
        both_hits
            .iter()
            .all(|(origin, _)| *origin != root_without_edges),
        "the requested zero-edge root must not acquire another root's rows"
    );
}

#[tokio::test]
async fn batch_neighbors_preserves_duplicate_requested_sources() {
    let store = setup_memory_store();
    let root = Uuid::new_v4();
    let target = Uuid::new_v4();
    let edge = make_edge(root, target, EdgeRelation::Extends, 1.0);
    let edge_id = Uuid::from(edge.id);
    store.upsert_edge(edge).await.unwrap();

    let hits = store
        .batch_neighbors(
            &[root, root],
            NeighborQuery {
                direction: Direction::Out,
                relations: None,
                limit: None,
                min_weight: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(
        neighbor_edge_multiset(&hits),
        HashMap::from([((root, edge_id), 2)]),
        "the optimized override must match the trait default's per-occurrence output"
    );
}

/// PARITY REGRESSION GUARD — the critical bug (HIGH).
/// For Direction::Both + limit=Some(1), batch_neighbors must return AT MOST
/// `limit` hits per source, not up to 2× (one per direction).
/// Before the fix, Both ran Out and In separately and concatenated, so a node
/// with ≥1 outgoing AND ≥1 incoming edge would yield 2 hits when limit=1.
// `#[serial(neighbor_select_count)]`: see note on `test_neighbors_outbound`.
#[tokio::test]
#[serial(neighbor_select_count)]
async fn batch_neighbors_both_limit_matches_single_source_neighbors() {
    let store = setup_memory_store();
    // 2 outgoing, 2 incoming from centre
    let (centre, out_nodes, in_nodes) = build_star(&store, 2, 2).await;

    let q_both_limit1 = NeighborQuery {
        direction: Direction::Both,
        relations: None,
        limit: Some(1),
        min_weight: None,
    };

    // single-source neighbors() — ground truth
    let single_hits = store
        .neighbors(centre, q_both_limit1.clone())
        .await
        .unwrap();
    // batch_neighbors with the same query
    let batch_hits = store
        .batch_neighbors(&[centre], q_both_limit1.clone())
        .await
        .unwrap();

    assert_eq!(
        batch_hits.len(),
        single_hits.len(),
        "batch_neighbors Both+limit=1 must return same count as neighbors() \
         (was 2× before fix)"
    );

    // Sanity: single-source also returns 1 when limit=1
    assert_eq!(single_hits.len(), 1, "neighbors() must respect limit=1");

    // Ensure the returned node is one of the actual neighbours.
    let all_neighbours: HashSet<Uuid> = out_nodes.iter().chain(in_nodes.iter()).copied().collect();
    let batch_node_ids: HashSet<Uuid> = batch_hits.iter().map(|(_, h)| h.node_id).collect();
    for nid in &batch_node_ids {
        assert!(
            all_neighbours.contains(nid),
            "batch result must be a real neighbour of centre"
        );
    }
}

/// PARITY: set equality for Out direction, with and without limit.
// `#[serial(neighbor_select_count)]`: see note on `test_neighbors_outbound`.
#[tokio::test]
#[serial(neighbor_select_count)]
async fn batch_neighbors_out_parity_with_neighbors() {
    let store = setup_memory_store();
    let (centre, out_nodes, _) = build_star(&store, 3, 2).await;

    let q_out = NeighborQuery {
        direction: Direction::Out,
        relations: None,
        limit: None,
        min_weight: None,
    };

    let single: HashSet<Uuid> =
        single_neighbour_set(&store.neighbors(centre, q_out.clone()).await.unwrap());
    let batch: HashSet<Uuid> = neighbour_set(
        &store
            .batch_neighbors(&[centre], q_out.clone())
            .await
            .unwrap(),
    );
    assert_eq!(batch, single, "Out: batch must equal single-source set");

    let expected: HashSet<Uuid> = out_nodes.iter().copied().collect();
    assert_eq!(
        batch, expected,
        "Out: must return exactly the out-neighbours"
    );
}

/// PARITY: set equality for In direction.
// `#[serial(neighbor_select_count)]`: see note on `test_neighbors_outbound`.
#[tokio::test]
#[serial(neighbor_select_count)]
async fn batch_neighbors_in_parity_with_neighbors() {
    let store = setup_memory_store();
    let (centre, _, in_nodes) = build_star(&store, 2, 3).await;

    let q_in = NeighborQuery {
        direction: Direction::In,
        relations: None,
        limit: None,
        min_weight: None,
    };

    let single: HashSet<Uuid> =
        single_neighbour_set(&store.neighbors(centre, q_in.clone()).await.unwrap());
    let batch: HashSet<Uuid> = neighbour_set(
        &store
            .batch_neighbors(&[centre], q_in.clone())
            .await
            .unwrap(),
    );
    assert_eq!(batch, single, "In: batch must equal single-source set");

    let expected: HashSet<Uuid> = in_nodes.iter().copied().collect();
    assert_eq!(batch, expected, "In: must return exactly the in-neighbours");
}

/// PARITY: set equality for Both direction, no limit.
// `#[serial(neighbor_select_count)]`: see note on `test_neighbors_outbound`.
#[tokio::test]
#[serial(neighbor_select_count)]
async fn batch_neighbors_both_parity_no_limit() {
    let store = setup_memory_store();
    let (centre, out_nodes, in_nodes) = build_star(&store, 2, 3).await;

    let q_both = NeighborQuery {
        direction: Direction::Both,
        relations: None,
        limit: None,
        min_weight: None,
    };

    let single: HashSet<Uuid> =
        single_neighbour_set(&store.neighbors(centre, q_both.clone()).await.unwrap());
    let batch: HashSet<Uuid> = neighbour_set(
        &store
            .batch_neighbors(&[centre], q_both.clone())
            .await
            .unwrap(),
    );
    assert_eq!(batch, single, "Both: batch must equal single-source set");

    let expected: HashSet<Uuid> = out_nodes.iter().chain(in_nodes.iter()).copied().collect();
    assert_eq!(batch, expected, "Both: must return all neighbours");
}

/// PARITY: relations filter applies correctly across directions.
// `#[serial(neighbor_select_count)]`: see note on `test_neighbors_outbound`.
#[tokio::test]
#[serial(neighbor_select_count)]
async fn batch_neighbors_relations_filter_parity() {
    let store = setup_memory_store();
    let centre = Uuid::new_v4();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    // outgoing: one Extends, one DependsOn
    store
        .upsert_edge(make_edge(centre, a, EdgeRelation::Extends, 1.0))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(centre, b, EdgeRelation::DependsOn, 1.0))
        .await
        .unwrap();

    let q = NeighborQuery {
        direction: Direction::Out,
        relations: Some(vec![EdgeRelation::Extends]),
        limit: None,
        min_weight: None,
    };

    let single: HashSet<Uuid> =
        single_neighbour_set(&store.neighbors(centre, q.clone()).await.unwrap());
    let batch: HashSet<Uuid> = neighbour_set(&store.batch_neighbors(&[centre], q).await.unwrap());

    assert_eq!(batch, single, "relations filter: batch must match single");
    assert!(
        batch.contains(&a),
        "filtered result must include Extends target"
    );
    assert!(
        !batch.contains(&b),
        "filtered result must exclude DependsOn target"
    );
}

/// PARITY: min_weight filter applies correctly.
// `#[serial(neighbor_select_count)]`: see note on `test_neighbors_outbound`.
#[tokio::test]
#[serial(neighbor_select_count)]
async fn batch_neighbors_min_weight_filter_parity() {
    let store = setup_memory_store();
    let centre = Uuid::new_v4();
    let heavy = Uuid::new_v4();
    let light = Uuid::new_v4();
    store
        .upsert_edge(make_edge(centre, heavy, EdgeRelation::Extends, 0.9))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(centre, light, EdgeRelation::Extends, 0.3))
        .await
        .unwrap();

    let q = NeighborQuery {
        direction: Direction::Out,
        relations: None,
        limit: None,
        min_weight: Some(0.5),
    };

    let single: HashSet<Uuid> =
        single_neighbour_set(&store.neighbors(centre, q.clone()).await.unwrap());
    let batch: HashSet<Uuid> = neighbour_set(&store.batch_neighbors(&[centre], q).await.unwrap());

    assert_eq!(batch, single, "min_weight filter: batch must match single");
    assert!(
        batch.contains(&heavy),
        "must include edge above weight threshold"
    );
    assert!(
        !batch.contains(&light),
        "must exclude edge below weight threshold"
    );
}
