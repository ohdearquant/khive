// `#[serial(neighbor_select_count)]`: shares the key with the tests that
// assert on the process-wide `NEIGHBOR_SELECT_COUNT` so a concurrent
// `neighbors()` call from this test can't corrupt their count.
#[tokio::test]
#[serial(neighbor_select_count)]
async fn test_neighbors_outbound() {
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
        .upsert_edge(make_edge(a, c, EdgeRelation::DependsOn, 0.7))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(d, a, EdgeRelation::Extends, 0.5))
        .await
        .unwrap();

    let query = NeighborQuery {
        direction: Direction::Out,
        relations: None,
        limit: None,
        min_weight: None,
    };

    let hits = store.neighbors(a, query).await.unwrap();
    assert_eq!(hits.len(), 2);

    let neighbor_ids: Vec<Uuid> = hits.iter().map(|h| h.node_id).collect();
    assert!(neighbor_ids.contains(&b));
    assert!(neighbor_ids.contains(&c));
    assert!(!neighbor_ids.contains(&d));
}

/// Regression guard (ADR-089 context-verb review): a
/// `limit` narrower than the neighbor set must keep the highest-weight edges,
/// not an arbitrary SQLite row-order subset. Before the fix, `neighbors()`
/// applied `LIMIT` with no `ORDER BY`, so a low-weight neighbor could win over
/// a high-weight one purely by insertion order.
// `#[serial(neighbor_select_count)]`: see note on `test_neighbors_outbound`.
#[tokio::test]
#[serial(neighbor_select_count)]
async fn test_neighbors_limit_keeps_highest_weight_not_insertion_order() {
    let store = setup_memory_store();

    let centre = Uuid::new_v4();
    let low = Uuid::new_v4();
    let mid = Uuid::new_v4();
    let high = Uuid::new_v4();

    // Insert lowest-weight edge FIRST so insertion order and weight order
    // disagree — if LIMIT ignores ORDER BY, the low-weight edge can win.
    store
        .upsert_edge(make_edge(centre, low, EdgeRelation::Extends, 0.1))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(centre, mid, EdgeRelation::Extends, 0.5))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(centre, high, EdgeRelation::Extends, 0.9))
        .await
        .unwrap();

    let query = NeighborQuery {
        direction: Direction::Out,
        relations: None,
        limit: Some(2),
        min_weight: None,
    };

    let hits = store.neighbors(centre, query).await.unwrap();
    assert_eq!(hits.len(), 2, "limit=2 must return exactly 2 neighbors");

    let neighbor_ids: HashSet<Uuid> = hits.iter().map(|h| h.node_id).collect();
    assert!(
        neighbor_ids.contains(&high),
        "highest-weight neighbor must survive a narrowing limit"
    );
    assert!(
        neighbor_ids.contains(&mid),
        "second-highest-weight neighbor must survive a narrowing limit"
    );
    assert!(
        !neighbor_ids.contains(&low),
        "lowest-weight neighbor must be the one dropped by limit"
    );

    // Weight-descending, node_id-ascending-tiebreak ordering must also hold
    // for the returned rows themselves (ADR-089's neighbor-ordering contract).
    assert!((hits[0].weight - 0.9).abs() < 1e-9);
    assert!((hits[1].weight - 0.5).abs() < 1e-9);
}

/// Correctness parity (ADR-089 context-verb optimization): `neighbors_both_directions`
/// must return exactly the union of a separate `Out` call and a separate `In`
/// call — same node/edge/relation/weight set, same direction tag per hit, and
/// the same global weight-descending/node_id-ascending order — while doing it
/// in one storage query instead of two. Outgoing and incoming weights
/// interleave (0.9, 0.6, 0.3 outgoing vs 0.8, 0.4 incoming) so the assertion
/// proves global post-union ordering, not per-branch order.
// `#[serial(neighbor_select_count)]`: see note on `test_neighbors_outbound`.
#[tokio::test]
#[serial(neighbor_select_count)]
async fn test_neighbors_both_directions_matches_two_separate_calls_and_order() {
    let store = setup_memory_store();

    let centre = Uuid::new_v4();
    let out_hi = Uuid::new_v4();
    let out_mid = Uuid::new_v4();
    let out_lo = Uuid::new_v4();
    let in_hi = Uuid::new_v4();
    let in_mid = Uuid::new_v4();

    store
        .upsert_edge(make_edge(centre, out_hi, EdgeRelation::Extends, 0.9))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(in_hi, centre, EdgeRelation::Extends, 0.8))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(centre, out_mid, EdgeRelation::Extends, 0.6))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(in_mid, centre, EdgeRelation::Extends, 0.4))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(centre, out_lo, EdgeRelation::Extends, 0.3))
        .await
        .unwrap();

    let directed = store
        .neighbors_both_directions(
            centre,
            NeighborQuery {
                direction: Direction::Both,
                relations: None,
                limit: None,
                min_weight: None,
            },
        )
        .await
        .unwrap();

    // Global weight-descending order across BOTH directions, not per-branch:
    // 0.9(out) > 0.8(in) > 0.6(out) > 0.4(in) > 0.3(out).
    let got: Vec<(Uuid, f64, Direction)> = directed
        .iter()
        .map(|d| (d.hit.node_id, d.hit.weight, d.direction.clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            (out_hi, 0.9, Direction::Out),
            (in_hi, 0.8, Direction::In),
            (out_mid, 0.6, Direction::Out),
            (in_mid, 0.4, Direction::In),
            (out_lo, 0.3, Direction::Out),
        ],
        "neighbors_both_directions must interleave by global weight DESC, tagging each hit's real direction"
    );

    // Parity: the same result set must be reconstructable from two separate
    // direction-scoped `neighbors()` calls (the pre-optimization behavior).
    let out_hits = store
        .neighbors(
            centre,
            NeighborQuery {
                direction: Direction::Out,
                relations: None,
                limit: None,
                min_weight: None,
            },
        )
        .await
        .unwrap();
    let in_hits = store
        .neighbors(
            centre,
            NeighborQuery {
                direction: Direction::In,
                relations: None,
                limit: None,
                min_weight: None,
            },
        )
        .await
        .unwrap();

    let directed_out: HashSet<(Uuid, Uuid)> = directed
        .iter()
        .filter(|d| d.direction == Direction::Out)
        .map(|d| (d.hit.node_id, d.hit.edge_id))
        .collect();
    let directed_in: HashSet<(Uuid, Uuid)> = directed
        .iter()
        .filter(|d| d.direction == Direction::In)
        .map(|d| (d.hit.node_id, d.hit.edge_id))
        .collect();
    let plain_out: HashSet<(Uuid, Uuid)> =
        out_hits.iter().map(|h| (h.node_id, h.edge_id)).collect();
    let plain_in: HashSet<(Uuid, Uuid)> = in_hits.iter().map(|h| (h.node_id, h.edge_id)).collect();
    assert_eq!(
        directed_out, plain_out,
        "outgoing subset must match a separate Out call"
    );
    assert_eq!(
        directed_in, plain_in,
        "incoming subset must match a separate In call"
    );
}

/// Tight `fanout`/`limit` parity: with a narrowing limit, `neighbors_both_directions`
/// must keep the same top-K set (by global weight) that the pre-optimization
/// two-call-then-merge-then-truncate approach kept.
// `#[serial(neighbor_select_count)]`: see note on `test_neighbors_outbound`.
#[tokio::test]
#[serial(neighbor_select_count)]
async fn test_neighbors_both_directions_limit_keeps_global_top_k() {
    let store = setup_memory_store();

    let centre = Uuid::new_v4();
    let out_hi = Uuid::new_v4();
    let in_hi = Uuid::new_v4();
    let out_lo = Uuid::new_v4();
    let in_lo = Uuid::new_v4();

    store
        .upsert_edge(make_edge(centre, out_hi, EdgeRelation::Extends, 0.95))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(in_hi, centre, EdgeRelation::Extends, 0.85))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(centre, out_lo, EdgeRelation::Extends, 0.2))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(in_lo, centre, EdgeRelation::Extends, 0.1))
        .await
        .unwrap();

    let directed = store
        .neighbors_both_directions(
            centre,
            NeighborQuery {
                direction: Direction::Both,
                relations: None,
                limit: Some(2),
                min_weight: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(
        directed.len(),
        2,
        "limit=2 must cap at 2 across both directions"
    );
    let ids: Vec<Uuid> = directed.iter().map(|d| d.hit.node_id).collect();
    assert_eq!(
        ids,
        vec![out_hi, in_hi],
        "the two globally-highest-weight neighbors must survive, in weight-descending order"
    );
    assert_eq!(directed[0].direction, Direction::Out);
    assert_eq!(directed[1].direction, Direction::In);
}
