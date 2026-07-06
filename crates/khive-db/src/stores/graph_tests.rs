use super::*;
use crate::pool::PoolConfig;
use khive_storage::types::{Direction, TraversalOptions};
use serial_test::serial;
use std::collections::HashSet;

fn setup_memory_store() -> SqlGraphStore {
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());

    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(GRAPH_DDL).unwrap();
    }

    SqlGraphStore::new_scoped(pool, false, "default")
}

fn make_edge(source: Uuid, target: Uuid, relation: EdgeRelation, weight: f64) -> Edge {
    let now = Utc::now();
    Edge {
        id: Uuid::new_v4().into(),
        namespace: "default".to_string(),
        source_id: source,
        target_id: target,
        relation,
        weight,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        metadata: None,
        target_backend: None,
    }
}

#[tokio::test]
async fn test_upsert_and_get_edge() {
    let store = setup_memory_store();

    let src = Uuid::new_v4();
    let tgt = Uuid::new_v4();
    let now = Utc::now();
    let edge = Edge {
        id: Uuid::new_v4().into(),
        namespace: "default".to_string(),
        source_id: src,
        target_id: tgt,
        relation: EdgeRelation::Extends,
        weight: 0.8,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        metadata: None,
        target_backend: None,
    };
    let edge_id = edge.id;

    store.upsert_edge(edge).await.unwrap();

    let fetched = store.get_edge(edge_id).await.unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.id, edge_id);
    assert_eq!(fetched.namespace, "default");
    assert_eq!(fetched.source_id, src);
    assert_eq!(fetched.target_id, tgt);
    assert_eq!(fetched.relation, EdgeRelation::Extends);
    assert!((fetched.weight - 0.8).abs() < 1e-9);
}

#[tokio::test]
async fn test_delete_edge() {
    let store = setup_memory_store();

    let edge = make_edge(Uuid::new_v4(), Uuid::new_v4(), EdgeRelation::Contains, 1.0);
    let edge_id = edge.id;

    store.upsert_edge(edge).await.unwrap();
    assert!(store.get_edge(edge_id).await.unwrap().is_some());

    let deleted = store.delete_edge(edge_id, DeleteMode::Hard).await.unwrap();
    assert!(deleted);

    assert!(store.get_edge(edge_id).await.unwrap().is_none());

    let deleted_again = store.delete_edge(edge_id, DeleteMode::Hard).await.unwrap();
    assert!(!deleted_again);
}

#[tokio::test]
async fn test_count_edges() {
    let store = setup_memory_store();

    assert_eq!(store.count_edges(EdgeFilter::default()).await.unwrap(), 0);

    for _ in 0..5 {
        store
            .upsert_edge(make_edge(
                Uuid::new_v4(),
                Uuid::new_v4(),
                EdgeRelation::DependsOn,
                1.0,
            ))
            .await
            .unwrap();
    }

    assert_eq!(store.count_edges(EdgeFilter::default()).await.unwrap(), 5);
}

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

/// Regression guard (ADR-089 context-verb review, internal review round 1, High-1): a
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

/// Count-falsifiable A/B proof (ADR-089 context-verb optimization): a
/// `direction="both"` neighbor fetch issues exactly ONE storage `neighbors`
/// SELECT via `neighbors_both_directions`, versus TWO if a caller instead
/// issued separate `Out` and `In` calls (the pre-fix `context` handler
/// pattern this replaces — see `fetch_directed_neighbors` in
/// `khive-pack-kg/src/handlers/context.rs`). For N expanded nodes across V
/// visible namespaces this is the `2*N*V -> 1*N*V` reduction described in the
/// verification verdict.
///
/// `#[serial(neighbor_select_count)]`: `NEIGHBOR_SELECT_COUNT` is a process-wide
/// counter — any other test issuing a neighbor SELECT while this one resets
/// and checks it would corrupt the count under the default parallel test runner.
#[tokio::test]
#[serial(neighbor_select_count)]
async fn test_neighbors_both_directions_halves_storage_query_count() {
    let store = setup_memory_store();
    let centre = Uuid::new_v4();
    let neighbor = Uuid::new_v4();
    store
        .upsert_edge(make_edge(centre, neighbor, EdgeRelation::Extends, 0.5))
        .await
        .unwrap();

    let query = NeighborQuery {
        direction: Direction::Both,
        relations: None,
        limit: None,
        min_weight: None,
    };

    reset_neighbor_select_count();
    store
        .neighbors_both_directions(centre, query.clone())
        .await
        .unwrap();
    assert_eq!(
        neighbor_select_count(),
        1,
        "one neighbors_both_directions call must issue exactly 1 storage SELECT"
    );

    reset_neighbor_select_count();
    let out_query = NeighborQuery {
        direction: Direction::Out,
        ..query.clone()
    };
    let in_query = NeighborQuery {
        direction: Direction::In,
        ..query
    };
    store.neighbors(centre, out_query).await.unwrap();
    store.neighbors(centre, in_query).await.unwrap();
    assert_eq!(
        neighbor_select_count(),
        2,
        "the old pattern of two direction-scoped neighbors() calls issues 2 SELECTs — \
         exactly the query count neighbors_both_directions halves"
    );
}

/// Reciprocal equal-weight determinism (internal review round 2, High): a
/// node with an Out edge to a neighbor and an In edge from the SAME neighbor
/// at the SAME weight ties on `(weight, node_id)` — the pre-fix `ORDER BY`.
/// Under a tight `limit`, the surviving row must be picked by a deterministic
/// tie-break (the `out` row wins), not by whichever row SQLite happens to
/// return first. Repeated to demonstrate the result is stable across calls.
///
/// `#[serial(neighbor_select_count)]`: shares the key with
/// `test_neighbors_both_directions_halves_storage_query_count` so this test's
/// repeated `neighbors_both_directions` calls can't corrupt that test's count
/// of the process-wide `NEIGHBOR_SELECT_COUNT`.
#[tokio::test]
#[serial(neighbor_select_count)]
async fn test_neighbors_both_directions_reciprocal_equal_weight_limit_is_deterministic() {
    let store = setup_memory_store();

    let centre = Uuid::new_v4();
    let other = Uuid::new_v4();

    store
        .upsert_edge(make_edge(centre, other, EdgeRelation::Extends, 0.5))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(other, centre, EdgeRelation::Extends, 0.5))
        .await
        .unwrap();

    let query = NeighborQuery {
        direction: Direction::Both,
        relations: None,
        limit: Some(1),
        min_weight: None,
    };

    for _ in 0..5 {
        let directed = store
            .neighbors_both_directions(centre, query.clone())
            .await
            .unwrap();
        assert_eq!(
            directed.len(),
            1,
            "limit=1 must return exactly one hit even with a reciprocal equal-weight pair"
        );
        assert_eq!(directed[0].hit.node_id, other);
        assert_eq!(
            directed[0].direction,
            Direction::Out,
            "the out-direction row must win the weight/node_id tie deterministically"
        );
    }
}

/// Forward-regression contract for `neighbors_both_directions`'s pre-`LIMIT`
/// tie-break order: `weight DESC, node_id ASC, CASE dir WHEN 'out' THEN 0 ELSE
/// 1 END ASC, edge_id ASC`. Four edges tie on `(weight, node_id)` — two Out
/// and two In, all to/from the same neighbor — with edge_ids chosen so every
/// Out edge_id sorts below every In edge_id, and inserted in an order that
/// does not match the expected result order. That means neither the
/// direction rank nor the `edge_id` tie-break can be satisfied by accidental
/// insertion-order preservation: only the explicit `ORDER BY` can produce the
/// sequence asserted below. A future change that reverses the direction rank
/// (`in` before `out`) or flips `edge_id ASC` to `DESC` yields a different
/// sequence than the one asserted here for either component, so this test
/// fails the moment either tie-break regresses.
///
/// The two edges sharing a direction use different relations (`Extends` /
/// `DependsOn`) purely to satisfy the storage layer's
/// `(namespace, source_id, target_id, relation)` uniqueness constraint — a
/// second edge with the same relation between the same ordered pair upserts
/// onto the first rather than creating a second row. The relation choice is
/// not part of the ordering contract under test.
///
/// `#[serial(neighbor_select_count)]`: see note on `test_neighbors_outbound`.
#[tokio::test]
#[serial(neighbor_select_count)]
async fn test_neighbors_both_directions_direction_then_edge_id_is_a_forward_contract() {
    let store = setup_memory_store();

    let centre = Uuid::new_v4();
    let other = Uuid::new_v4();

    let out_lo: Uuid = "00000000-0000-0000-0000-000000000001".parse().unwrap();
    let out_hi: Uuid = "00000000-0000-0000-0000-000000000002".parse().unwrap();
    let in_lo: Uuid = "00000000-0000-0000-0000-000000000003".parse().unwrap();
    let in_hi: Uuid = "00000000-0000-0000-0000-000000000004".parse().unwrap();

    // Inserted out of both the weight/direction tie-break order and the
    // edge_id order, so nothing about the asserted sequence below can pass
    // by coincidentally preserving insertion order.
    for (id, source, target, relation) in [
        (in_lo, other, centre, EdgeRelation::Extends),
        (out_hi, centre, other, EdgeRelation::DependsOn),
        (in_hi, other, centre, EdgeRelation::DependsOn),
        (out_lo, centre, other, EdgeRelation::Extends),
    ] {
        let now = Utc::now();
        store
            .upsert_edge(Edge {
                id: id.into(),
                namespace: "default".to_string(),
                source_id: source,
                target_id: target,
                relation,
                weight: 0.5,
                created_at: now,
                updated_at: now,
                deleted_at: None,
                metadata: None,
                target_backend: None,
            })
            .await
            .unwrap();
    }

    let query = NeighborQuery {
        direction: Direction::Both,
        relations: None,
        limit: None,
        min_weight: None,
    };

    let unlimited = store
        .neighbors_both_directions(centre, query)
        .await
        .unwrap();

    let observed: Vec<(Direction, Uuid)> = unlimited
        .iter()
        .map(|h| (h.direction.clone(), h.hit.edge_id))
        .collect();
    assert_eq!(
        observed,
        vec![
            (Direction::Out, out_lo),
            (Direction::Out, out_hi),
            (Direction::In, in_lo),
            (Direction::In, in_hi),
        ],
        "tied (weight, node_id) rows must order Out-before-In, then edge_id ASC within each direction"
    );

    // The storage layer applies `LIMIT` to this pre-sorted order (the runtime
    // caller only re-sorts after `LIMIT` has already truncated), so survivors
    // under a narrowing limit must be a prefix of the sequence above, not an
    // arbitrary 2-of-4 subset.
    let limited_query = NeighborQuery {
        direction: Direction::Both,
        relations: None,
        limit: Some(2),
        min_weight: None,
    };
    let limited = store
        .neighbors_both_directions(centre, limited_query)
        .await
        .unwrap();
    let limited_observed: Vec<(Direction, Uuid)> = limited
        .iter()
        .map(|h| (h.direction.clone(), h.hit.edge_id))
        .collect();
    assert_eq!(
        limited_observed,
        vec![(Direction::Out, out_lo), (Direction::Out, out_hi)],
        "limit=2 must keep exactly the first two rows of the deterministic order, both Out"
    );
}

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
/// exactly once in the result (#285).
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

/// First-visit (BFS) ordering is deterministic: the node seen at the
/// shallowest depth wins, and the `via_edge` recorded for it is the one
/// from that first-visited path.
///
/// Graph: A→B (depth 1), A→C (depth 1), B→D (depth 2), C→D (depth 2).
/// D appears at depth 2 via B or C.  Rows are ordered by depth; whichever
/// path SQLite enumerates first for depth-2 is the keeper.  The test
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

/// Multi-root batched traversal: two independent chains A→B→C and D→E→F.
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

/// Query-count regression: a 15-node binary tree at max_depth=3 must be
/// traversed in a single CTE execution (one conn.prepare call), not N CTEs.
/// This test asserts the node-count result is correct, which would fail if
/// the batched CTE produced duplicates or missed nodes.
#[tokio::test]
async fn test_traverse_binary_tree_result_count() {
    let store = setup_memory_store();

    // Build a complete binary tree of depth 3: root + 2 + 4 + 8 = 15 nodes.
    let nodes: Vec<Uuid> = (0..15).map(|_| Uuid::new_v4()).collect();
    for i in 0..7usize {
        let left = 2 * i + 1;
        let right = 2 * i + 2;
        store
            .upsert_edge(make_edge(nodes[i], nodes[left], EdgeRelation::Extends, 1.0))
            .await
            .unwrap();
        store
            .upsert_edge(make_edge(
                nodes[i],
                nodes[right],
                EdgeRelation::Extends,
                1.0,
            ))
            .await
            .unwrap();
    }

    let request = TraversalRequest {
        roots: vec![nodes[0]],
        options: TraversalOptions::new(3).with_direction(Direction::Out),
        include_roots: true,
        include_properties: false,
    };

    let paths = store.traverse(request).await.unwrap();
    assert_eq!(paths.len(), 1);
    // root + depth-1 (2) + depth-2 (4) + depth-3 (8) = 15
    assert_eq!(
        paths[0].nodes.len(),
        15,
        "binary tree depth-3 must yield exactly 15 nodes"
    );
    // Every depth-3 node must have a via_edge.
    for node in paths[0].nodes.iter().filter(|n| n.depth == 3) {
        assert!(
            node.via_edge.is_some(),
            "depth-3 nodes must carry a via_edge"
        );
    }
}

#[tokio::test]
async fn test_metadata_roundtrip() {
    let store = setup_memory_store();

    let src = Uuid::new_v4();
    let tgt = Uuid::new_v4();
    let meta = serde_json::json!({"note": "important link", "confidence": 0.95});
    let now = Utc::now();
    let edge = Edge {
        id: Uuid::new_v4().into(),
        namespace: "default".to_string(),
        source_id: src,
        target_id: tgt,
        relation: EdgeRelation::Implements,
        weight: 0.9,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        metadata: Some(meta.clone()),
        target_backend: None,
    };
    let edge_id = edge.id;

    store.upsert_edge(edge).await.unwrap();

    let fetched = store.get_edge(edge_id).await.unwrap().unwrap();
    assert_eq!(
        fetched.metadata.as_ref(),
        Some(&meta),
        "metadata must survive a write/read roundtrip via get_edge"
    );

    // Also verify via query_edges.
    let page = store
        .query_edges(EdgeFilter::default(), vec![], PageRequest::default())
        .await
        .unwrap();
    let from_query = page
        .items
        .iter()
        .find(|e| e.id == edge_id)
        .expect("edge must appear in query_edges result");
    assert_eq!(
        from_query.metadata.as_ref(),
        Some(&meta),
        "metadata must survive a write/read roundtrip via query_edges"
    );
}

#[tokio::test]
async fn test_upsert_edges_batch() {
    let store = setup_memory_store();

    let edges: Vec<Edge> = (0..10)
        .map(|i| {
            make_edge(
                Uuid::new_v4(),
                Uuid::new_v4(),
                EdgeRelation::Implements,
                i as f64,
            )
        })
        .collect();

    let summary = store.upsert_edges(edges).await.unwrap();
    assert_eq!(summary.attempted, 10);
    assert_eq!(summary.affected, 10);
    assert_eq!(summary.failed, 0);

    assert_eq!(store.count_edges(EdgeFilter::default()).await.unwrap(), 10);
}

// ---- #229 deduplication test ----

#[tokio::test]
async fn graph_duplicate_edges_ignored() {
    let store = setup_memory_store();

    let src = Uuid::new_v4();
    let tgt = Uuid::new_v4();

    // Two edges with the same (source_id, target_id, relation) triple but different IDs.
    let now = Utc::now();
    let edge1 = Edge {
        id: Uuid::new_v4().into(),
        namespace: "default".to_string(),
        source_id: src,
        target_id: tgt,
        relation: EdgeRelation::Extends,
        weight: 1.0,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        metadata: None,
        target_backend: None,
    };
    let edge2 = Edge {
        id: Uuid::new_v4().into(),
        namespace: "default".to_string(),
        source_id: src,
        target_id: tgt,
        relation: EdgeRelation::Extends,
        weight: 0.5,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        metadata: None,
        target_backend: None,
    };

    store.upsert_edge(edge1).await.unwrap();
    store.upsert_edge(edge2).await.unwrap();

    assert_eq!(
        store.count_edges(EdgeFilter::default()).await.unwrap(),
        1,
        "duplicate (source, target, relation) triple must be ignored; only one edge must exist"
    );
}

// F053 (CRIT): natural-key conflict must DO UPDATE (refresh weight/metadata), not DO NOTHING.
// The second upsert must overwrite weight=0.5; current code keeps weight=1.0.
#[tokio::test]
async fn graph_duplicate_edges_refresh_existing_row() {
    let store = setup_memory_store();
    let src = Uuid::new_v4();
    let tgt = Uuid::new_v4();

    let now = Utc::now();
    let edge1 = Edge {
        id: Uuid::new_v4().into(),
        namespace: "default".to_string(),
        source_id: src,
        target_id: tgt,
        relation: EdgeRelation::Extends,
        weight: 1.0,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        metadata: None,
        target_backend: None,
    };
    let edge2 = Edge {
        id: Uuid::new_v4().into(),
        namespace: "default".to_string(),
        source_id: src,
        target_id: tgt,
        relation: EdgeRelation::Extends,
        weight: 0.5,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        metadata: None,
        target_backend: None,
    };

    store.upsert_edge(edge1).await.unwrap();
    store.upsert_edge(edge2).await.unwrap();

    let edges = store
        .query_edges(EdgeFilter::default(), vec![], PageRequest::default())
        .await
        .unwrap();
    assert_eq!(
        edges.items.len(),
        1,
        "duplicate natural key must collapse to one row"
    );
    assert!(
        (edges.items[0].weight - 0.5).abs() < 0.001,
        "F053: natural-key conflict must DO UPDATE (weight=0.5 from second upsert); \
             current DO NOTHING keeps stale weight={}",
        edges.items[0].weight
    );
}

// Regression test for #476: symmetric edges stored via upsert_edge must
// always have source_id < target_id (lexicographic on UUID bytes).
#[tokio::test]
async fn upsert_edge_canonicalizes_symmetric_relation() {
    let store = setup_memory_store();

    // Construct two UUIDs where larger > smaller lexicographically.
    let smaller = Uuid::from_bytes([0x00; 16]);
    let larger = Uuid::from_bytes([0xff; 16]);
    assert!(
        larger > smaller,
        "test setup: larger must sort after smaller"
    );

    // Insert with source > target — the invariant-violating order.
    let edge = make_edge(larger, smaller, EdgeRelation::CompetesWith, 1.0);
    let edge_id = edge.id;
    store.upsert_edge(edge).await.unwrap();

    let stored = store.get_edge(edge_id).await.unwrap().unwrap();
    assert_eq!(
        stored.source_id, smaller,
        "#476: CompetesWith edge must be stored with source_id < target_id"
    );
    assert_eq!(
        stored.target_id, larger,
        "#476: CompetesWith edge must be stored with target_id > source_id"
    );
}

#[tokio::test]
async fn upsert_edges_batch_canonicalizes_symmetric_relation() {
    let store = setup_memory_store();

    let smaller = Uuid::from_bytes([0x11; 16]);
    let larger = Uuid::from_bytes([0xee; 16]);

    // ComposedWith is the other symmetric relation — insert reversed.
    let edge = make_edge(larger, smaller, EdgeRelation::ComposedWith, 0.9);
    let edge_id = edge.id;
    store.upsert_edges(vec![edge]).await.unwrap();

    let stored = store.get_edge(edge_id).await.unwrap().unwrap();
    assert_eq!(
        stored.source_id, smaller,
        "#476: ComposedWith edge must be stored with source_id < target_id (batch path)"
    );
    assert_eq!(
        stored.target_id, larger,
        "#476: ComposedWith edge must be stored with target_id > source_id (batch path)"
    );
}

#[tokio::test]
async fn upsert_edge_non_symmetric_relation_preserves_direction() {
    let store = setup_memory_store();

    // DependsOn is not symmetric — direction must NOT be swapped.
    let src = Uuid::from_bytes([0xff; 16]);
    let tgt = Uuid::from_bytes([0x00; 16]);
    let edge = make_edge(src, tgt, EdgeRelation::DependsOn, 1.0);
    let edge_id = edge.id;
    store.upsert_edge(edge).await.unwrap();

    let stored = store.get_edge(edge_id).await.unwrap().unwrap();
    assert_eq!(
        stored.source_id, src,
        "non-symmetric edge direction must be preserved"
    );
    assert_eq!(
        stored.target_id, tgt,
        "non-symmetric edge direction must be preserved"
    );
}

// ADR-007 PR-A1 regression: upsert must accept edges whose namespace differs
// from the store's construction-time namespace.  Before the fix, `upsert_edge`
// rejected this with InvalidInput; after, it stores the edge with whatever
// namespace the edge carries and get_edge finds it by UUID alone.
#[tokio::test]
async fn upsert_edge_cross_namespace_accepted() {
    // Store is constructed with "local" as its default multi-record query namespace.
    let store = setup_memory_store(); // uses "default" as construction namespace

    let src = Uuid::new_v4();
    let tgt = Uuid::new_v4();
    let now = Utc::now();

    // Edge carries "lambda:leo" — different from store's "default".
    let edge = Edge {
        id: Uuid::new_v4().into(),
        namespace: "lambda:leo".to_string(),
        source_id: src,
        target_id: tgt,
        relation: EdgeRelation::Extends,
        weight: 0.9,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        metadata: None,
        target_backend: None,
    };
    let edge_id = edge.id;

    // Must not error (V1 violation removed).
    store.upsert_edge(edge).await.unwrap();

    // get_edge by UUID must find it regardless of namespace.
    let stored = store.get_edge(edge_id).await.unwrap();
    assert!(
        stored.is_some(),
        "cross-namespace edge must be retrievable by UUID"
    );
    let stored = stored.unwrap();
    assert_eq!(
        stored.namespace, "lambda:leo",
        "namespace column must be preserved as stored"
    );
}

// ADR-007 PR-A1 regression: namespace column is preserved on the record even
// after upsert — not silently overwritten with the store's construction namespace.
#[tokio::test]
async fn upsert_edge_namespace_stored_on_record() {
    let store = setup_memory_store();

    let edge = make_edge(
        Uuid::new_v4(),
        Uuid::new_v4(),
        EdgeRelation::Implements,
        1.0,
    );
    let edge_id = edge.id;
    let ns = edge.namespace.clone();

    store.upsert_edge(edge).await.unwrap();

    let stored = store.get_edge(edge_id).await.unwrap().unwrap();
    assert_eq!(
        stored.namespace, ns,
        "namespace column must survive the write/read roundtrip"
    );
}

// ---- batch_neighbors parity tests (HIGH bug regression guard) ----

/// Build a star graph: centre node with `out_count` outgoing edges and
/// `in_count` incoming edges.  Returns (centre, out_targets, in_sources,
/// out_edge_ids, in_edge_ids).
async fn build_star(
    store: &SqlGraphStore,
    out_count: usize,
    in_count: usize,
) -> (Uuid, Vec<Uuid>, Vec<Uuid>) {
    let centre = Uuid::new_v4();
    let mut out_nodes = Vec::new();
    let mut in_nodes = Vec::new();
    for _ in 0..out_count {
        let tgt = Uuid::new_v4();
        store
            .upsert_edge(make_edge(centre, tgt, EdgeRelation::Extends, 1.0))
            .await
            .unwrap();
        out_nodes.push(tgt);
    }
    for _ in 0..in_count {
        let src = Uuid::new_v4();
        store
            .upsert_edge(make_edge(src, centre, EdgeRelation::Extends, 0.8))
            .await
            .unwrap();
        in_nodes.push(src);
    }
    (centre, out_nodes, in_nodes)
}

fn neighbour_set(hits: &[(Uuid, NeighborHit)]) -> HashSet<Uuid> {
    hits.iter().map(|(_, h)| h.node_id).collect()
}

fn single_neighbour_set(hits: &[NeighborHit]) -> HashSet<Uuid> {
    hits.iter().map(|h| h.node_id).collect()
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

/// Regression guard (issue #589): a `limit` narrower than an origin's full
/// neighbor set must keep that origin's highest-weight edges, not an
/// arbitrary SQLite row-order subset. Before the fix, the `ROW_NUMBER()
/// OVER (PARTITION BY origin_id)` window had no `ORDER BY`, so a low-weight
/// neighbor could win over a high-weight one purely by insertion order —
/// and because this is per-origin, one origin's row order could disagree
/// with another's, so the test uses two origins with the low/mid/high
/// insertion order reversed between them.
#[tokio::test]
async fn batch_neighbors_limit_keeps_highest_weight_per_origin_not_insertion_order() {
    let store = setup_memory_store();

    let centre_a = Uuid::new_v4();
    let a_low = Uuid::new_v4();
    let a_mid = Uuid::new_v4();
    let a_high = Uuid::new_v4();

    let centre_b = Uuid::new_v4();
    let b_low = Uuid::new_v4();
    let b_mid = Uuid::new_v4();
    let b_high = Uuid::new_v4();

    // centre_a: insert lowest-weight edge FIRST (insertion order disagrees
    // with weight order).
    store
        .upsert_edge(make_edge(centre_a, a_low, EdgeRelation::Extends, 0.1))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(centre_a, a_mid, EdgeRelation::Extends, 0.5))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(centre_a, a_high, EdgeRelation::Extends, 0.9))
        .await
        .unwrap();

    // centre_b: insert highest-weight edge FIRST — the reverse of centre_a —
    // so a single global insertion-order artifact cannot accidentally pass
    // both origins.
    store
        .upsert_edge(make_edge(centre_b, b_high, EdgeRelation::Extends, 0.9))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(centre_b, b_mid, EdgeRelation::Extends, 0.5))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(centre_b, b_low, EdgeRelation::Extends, 0.1))
        .await
        .unwrap();

    let query = NeighborQuery {
        direction: Direction::Out,
        relations: None,
        limit: Some(2),
        min_weight: None,
    };

    let hits = store
        .batch_neighbors(&[centre_a, centre_b], query)
        .await
        .unwrap();

    let a_hits: Vec<&NeighborHit> = hits
        .iter()
        .filter(|(origin, _)| *origin == centre_a)
        .map(|(_, h)| h)
        .collect();
    let b_hits: Vec<&NeighborHit> = hits
        .iter()
        .filter(|(origin, _)| *origin == centre_b)
        .map(|(_, h)| h)
        .collect();

    assert_eq!(
        a_hits.len(),
        2,
        "centre_a: per-origin limit=2 must return exactly 2 neighbors"
    );
    assert_eq!(
        b_hits.len(),
        2,
        "centre_b: per-origin limit=2 must return exactly 2 neighbors"
    );

    let a_ids: HashSet<Uuid> = a_hits.iter().map(|h| h.node_id).collect();
    assert!(
        a_ids.contains(&a_high),
        "centre_a: highest-weight neighbor must survive a narrowing limit"
    );
    assert!(
        a_ids.contains(&a_mid),
        "centre_a: second-highest-weight neighbor must survive a narrowing limit"
    );
    assert!(
        !a_ids.contains(&a_low),
        "centre_a: lowest-weight neighbor must be the one dropped by limit"
    );

    let b_ids: HashSet<Uuid> = b_hits.iter().map(|h| h.node_id).collect();
    assert!(
        b_ids.contains(&b_high),
        "centre_b: highest-weight neighbor must survive a narrowing limit"
    );
    assert!(
        b_ids.contains(&b_mid),
        "centre_b: second-highest-weight neighbor must survive a narrowing limit"
    );
    assert!(
        !b_ids.contains(&b_low),
        "centre_b: lowest-weight neighbor must be the one dropped by limit"
    );
}

// ---- get_edges direct SQLite tests ----

/// get_edges must return all requested edges regardless of request order.
#[tokio::test]
async fn get_edges_order_independent() {
    let store = setup_memory_store();

    let edges: Vec<Edge> = (0..5)
        .map(|i| {
            make_edge(
                Uuid::new_v4(),
                Uuid::new_v4(),
                EdgeRelation::Extends,
                i as f64,
            )
        })
        .collect();
    let ids: Vec<LinkId> = edges.iter().map(|e| e.id).collect();
    for e in edges {
        store.upsert_edge(e).await.unwrap();
    }

    // Request in reversed order
    let mut reversed = ids.clone();
    reversed.reverse();

    let result = store.get_edges(&reversed).await.unwrap();
    let result_ids: HashSet<LinkId> = result.iter().map(|e| e.id).collect();
    let expected_ids: HashSet<LinkId> = ids.iter().copied().collect();
    assert_eq!(
        result_ids, expected_ids,
        "get_edges must return all edges regardless of request order"
    );
}

/// Soft-deleted or nonexistent IDs are silently omitted.
#[tokio::test]
async fn get_edges_omits_deleted_and_missing() {
    let store = setup_memory_store();

    let live = make_edge(Uuid::new_v4(), Uuid::new_v4(), EdgeRelation::Extends, 1.0);
    let soft = make_edge(Uuid::new_v4(), Uuid::new_v4(), EdgeRelation::DependsOn, 0.5);
    let ghost_id = LinkId::from(Uuid::new_v4()); // never inserted

    let live_id = live.id;
    let soft_id = soft.id;

    store.upsert_edge(live).await.unwrap();
    store.upsert_edge(soft).await.unwrap();
    // Soft-delete the second edge
    store.delete_edge(soft_id, DeleteMode::Soft).await.unwrap();

    let result = store
        .get_edges(&[live_id, soft_id, ghost_id])
        .await
        .unwrap();

    assert_eq!(result.len(), 1, "only the live edge must be returned");
    assert_eq!(result[0].id, live_id, "returned edge must be the live one");
}

/// get_edges with more than 900 IDs (chunk boundary) returns all live edges.
#[tokio::test]
async fn get_edges_chunk_boundary() {
    let store = setup_memory_store();

    let count = 950usize;
    let edges: Vec<Edge> = (0..count)
        .map(|_| make_edge(Uuid::new_v4(), Uuid::new_v4(), EdgeRelation::Extends, 1.0))
        .collect();
    let ids: Vec<LinkId> = edges.iter().map(|e| e.id).collect();
    store.upsert_edges(edges).await.unwrap();

    let result = store.get_edges(&ids).await.unwrap();
    assert_eq!(
        result.len(),
        count,
        "get_edges must return all {count} edges across the chunk boundary"
    );
}

/// batch_neighbors Direction::Both chunk-boundary test.
///
/// With the old const CHUNK=880, Direction::Both would bind ~1761 variables
/// (1 ns + 880 out_srcs + 880 in_srcs) into a single SQLite statement,
/// blowing past SQLITE_MAX_VARIABLE_NUMBER=999 and returning an error.
///
/// This test uses 500 source nodes — enough that a single Both chunk would
/// have exceeded 999 variables under the old constant.  After the fix the
/// computed chunk_size for Both (no filters, no limit) is ~474, so the 500
/// sources are split into two chunks, each staying within budget.
///
/// Correctness: for a random sample of sources, batch result must equal the
/// per-source neighbors() result.
// `#[serial(neighbor_select_count)]`: see note on `test_neighbors_outbound`.
#[tokio::test]
#[serial(neighbor_select_count)]
async fn batch_neighbors_both_chunk_boundary() {
    let store = setup_memory_store();

    // Create 500 source nodes, each with one outgoing and one incoming edge.
    let source_count = 500usize;
    let mut sources: Vec<Uuid> = Vec::with_capacity(source_count);
    for _ in 0..source_count {
        let centre = Uuid::new_v4();
        let out_tgt = Uuid::new_v4();
        let in_src = Uuid::new_v4();
        store
            .upsert_edge(make_edge(centre, out_tgt, EdgeRelation::Extends, 1.0))
            .await
            .unwrap();
        store
            .upsert_edge(make_edge(in_src, centre, EdgeRelation::Extends, 0.8))
            .await
            .unwrap();
        sources.push(centre);
    }

    let q_both = NeighborQuery {
        direction: Direction::Both,
        relations: None,
        limit: None,
        min_weight: None,
    };

    // Must not error (would panic with "too many SQL variables" pre-fix).
    let batch_hits = store
        .batch_neighbors(&sources, q_both.clone())
        .await
        .unwrap();

    // Each source has exactly 2 neighbours (one out, one in).
    assert_eq!(
        batch_hits.len(),
        source_count * 2,
        "Both chunk-boundary: must return 2 hits per source (1 out + 1 in)"
    );

    // Spot-check: first, middle, and last source match per-source neighbors().
    for &idx in &[0, source_count / 2, source_count - 1] {
        let src = sources[idx];
        let single: HashSet<Uuid> = store
            .neighbors(src, q_both.clone())
            .await
            .unwrap()
            .into_iter()
            .map(|h| h.node_id)
            .collect();
        let from_batch: HashSet<Uuid> = batch_hits
            .iter()
            .filter(|(origin, _)| *origin == src)
            .map(|(_, h)| h.node_id)
            .collect();
        assert_eq!(
            from_batch, single,
            "spot-check source {idx}: batch result must match neighbors()"
        );
    }

    // Also verify with limit=1: each source gets at most 1 hit total.
    let q_limit = NeighborQuery {
        direction: Direction::Both,
        relations: None,
        limit: Some(1),
        min_weight: None,
    };
    let limited_hits = store.batch_neighbors(&sources, q_limit).await.unwrap();
    assert_eq!(
        limited_hits.len(),
        source_count,
        "Both chunk-boundary with limit=1: must return exactly 1 hit per source"
    );
}

// ---- per-root limit regression ----

/// Regression guard for the per-root `limit` regression introduced when N
/// roots were batched into a single CTE with one global SQL LIMIT.
///
/// Graph: A→B and C→D (two independent chains).  With `limit=1` and
/// `include_roots=false`, EVERY root must receive its own capped result —
/// not just the lexicographically-first root_id.
///
/// This test FAILs on the pre-fix code (global LIMIT returns 1 row total,
/// so only one root gets a path) and PASSes after the fix (Rust-level
/// per-root truncation after SQL returns all rows).
#[tokio::test]
async fn test_traverse_per_root_limit_capped_independently() {
    let store = setup_memory_store();

    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    let d = Uuid::new_v4();

    // Two independent one-hop chains: A→B and C→D.
    store
        .upsert_edge(make_edge(a, b, EdgeRelation::Extends, 1.0))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(c, d, EdgeRelation::Extends, 1.0))
        .await
        .unwrap();

    let request = TraversalRequest {
        roots: vec![a, c],
        options: TraversalOptions {
            max_depth: 2,
            direction: Direction::Out,
            relations: None,
            min_weight: None,
            limit: Some(1),
        },
        include_roots: false,
        include_properties: false,
    };

    let paths = store.traverse(request).await.unwrap();

    // Both roots must produce a path — global SQL LIMIT would drop one.
    assert_eq!(
        paths.len(),
        2,
        "both roots must produce a path even with limit=1 \
         (per-root cap, not global cap)"
    );

    // Each root gets exactly one node.
    for path in &paths {
        assert_eq!(
            path.nodes.len(),
            1,
            "root {:?}: limit=1 must cap to exactly one non-root node",
            path.root_id
        );
    }

    // Correct child per root.
    let path_a = paths.iter().find(|p| p.root_id == a).expect("path for A");
    let path_c = paths.iter().find(|p| p.root_id == c).expect("path for C");
    assert_eq!(path_a.nodes[0].node_id, b, "root A must reach child B");
    assert_eq!(path_c.nodes[0].node_id, d, "root C must reach child D");
}

// ---- batch == per-root decomposition equivalence tests ----

/// Equivalence fixture: for a small deterministic graph, batched
/// `traverse([R0, R1])` must produce the same per-root results as running
/// `traverse([R0])` and `traverse([R1])` independently, across four sections:
///
/// **Section A** – linear chains, Direction::Out, 6-case parameter matrix
///   (include_roots, relation filter, min_weight, finite limit):
///   A → B (w=0.9, Extends) → C (w=0.8) → D (w=0.7)
///   E → F (w=0.6, Extends) → G (w=0.5)
///
/// **Section B** – diamond with shortcut, Direction::Out (depth/via_edge drift):
///   H → VI (w=1.0, Extends) → K (w=1.0, Extends)
///   H → K  (w=0.5, PartOf)  ← shortcut; K is at depth 1 from H via H→K,
///                                          NOT depth 2 via H→VI→K.
///   Batched CTE must attribute K's first visit to depth=1, via_edge=H→K edge.
///
/// **Section C** – same diamond, Direction::In (bidirectional traversal):
///   roots [K, N].  K has two distinct incoming edges (H→K and VI→K); the
///   batched CTE must assign each one its own via_edge independently.
///
/// **Section D** – same diamond + chain, Direction::Both:
///   roots [VI, N].  VI has both in-edges (H→VI) and out-edges (VI→K).
///
/// M → N (w=1.0, Extends) — independent parallel chain used as the second
///   root in Sections B/C/D.
///
/// Same-depth node ordering is resolved by sorting PathNodes by (depth, node_id)
/// before the per-node comparison — no new ordering contract is introduced in
/// production code.
#[tokio::test]
async fn test_traverse_batch_equals_per_root_decomposition() {
    let store = setup_memory_store();

    // Section A nodes
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    let d = Uuid::new_v4();
    let e = Uuid::new_v4();
    let f = Uuid::new_v4();
    let g = Uuid::new_v4();

    // Section B/C/D nodes
    let h = Uuid::new_v4(); // diamond source
    let vi = Uuid::new_v4(); // diamond intermediate
    let k = Uuid::new_v4(); // convergent node (reachable from h directly AND via vi)
    let m = Uuid::new_v4(); // independent parallel root
    let n = Uuid::new_v4(); // child of m

    // Section A edges
    for (src, tgt, w) in [
        (a, b, 0.9_f64),
        (b, c, 0.8),
        (c, d, 0.7),
        (e, f, 0.6),
        (f, g, 0.5),
    ] {
        store
            .upsert_edge(make_edge(src, tgt, EdgeRelation::Extends, w))
            .await
            .unwrap();
    }

    // Section B/C/D edges
    store
        .upsert_edge(make_edge(h, vi, EdgeRelation::Extends, 1.0))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(vi, k, EdgeRelation::Extends, 1.0))
        .await
        .unwrap();
    // Shortcut: K is reachable from H in one hop; the depth-2 path via VI→K must
    // be suppressed by BFS first-visit in both batched and single-root traversals.
    store
        .upsert_edge(make_edge(h, k, EdgeRelation::PartOf, 0.5))
        .await
        .unwrap();
    store
        .upsert_edge(make_edge(m, n, EdgeRelation::Extends, 1.0))
        .await
        .unwrap();

    // Sort PathNodes by (depth, node_id) for a stable comparison even when
    // same-depth nodes appear in different orders between CTE executions.
    // This resolves depth-1 tie-breaking without requiring a new ordering
    // contract in production code.
    let sort_nodes = |nodes: &mut Vec<khive_storage::types::PathNode>| {
        nodes.sort_by_key(|n| (n.depth, n.node_id));
    };

    // Direction is Clone but not Copy; TraversalOptions is built per case.
    struct Case {
        include_roots: bool,
        direction: Direction,
        relation_filter: Option<EdgeRelation>,
        min_weight: Option<f64>,
        limit: Option<u32>,
    }

    // run_cases: for each case, assert batched([r0, r1]) == decomposed([r0]) + ([r1]).
    // Takes roots by value so it can be called for each section.
    async fn run_cases(
        store: &SqlGraphStore,
        root0: Uuid,
        root1: Uuid,
        cases: &[Case],
        sort_nodes: &dyn Fn(&mut Vec<khive_storage::types::PathNode>),
    ) {
        for case in cases {
            let opts = TraversalOptions {
                max_depth: 4,
                direction: case.direction.clone(),
                relations: case.relation_filter.map(|r| vec![r]),
                min_weight: case.min_weight,
                limit: case.limit,
            };

            let batched = store
                .traverse(TraversalRequest {
                    roots: vec![root0, root1],
                    options: opts.clone(),
                    include_roots: case.include_roots,
                    include_properties: false,
                })
                .await
                .unwrap();
            let single_0 = store
                .traverse(TraversalRequest {
                    roots: vec![root0],
                    options: opts.clone(),
                    include_roots: case.include_roots,
                    include_properties: false,
                })
                .await
                .unwrap();
            let single_1 = store
                .traverse(TraversalRequest {
                    roots: vec![root1],
                    options: opts,
                    include_roots: case.include_roots,
                    include_properties: false,
                })
                .await
                .unwrap();

            for (root_id, single_result) in [(root0, &single_0), (root1, &single_1)] {
                let batch_path = batched.iter().find(|p| p.root_id == root_id);
                let single_path = single_result.first();

                let label = format!(
                    "root={root_id:?} params=(include_roots={},dir={:?},rel={:?},\
                     min_w={:?},limit={:?})",
                    case.include_roots,
                    case.direction,
                    case.relation_filter,
                    case.min_weight,
                    case.limit,
                );

                match (batch_path, single_path) {
                    (None, None) => {}
                    (Some(bp), Some(sp)) => {
                        let mut bn = bp.nodes.clone();
                        let mut sn = sp.nodes.clone();
                        sort_nodes(&mut bn);
                        sort_nodes(&mut sn);

                        assert_eq!(
                            bn.len(),
                            sn.len(),
                            "{label}: node count mismatch batch={} single={}",
                            bn.len(),
                            sn.len()
                        );
                        for (bi, si) in bn.iter().zip(sn.iter()) {
                            assert_eq!(
                                bi.node_id, si.node_id,
                                "{label}: node_id mismatch at depth {}",
                                bi.depth
                            );
                            assert_eq!(
                                bi.depth, si.depth,
                                "{label}: depth mismatch for node {}",
                                bi.node_id
                            );
                            assert_eq!(
                                bi.via_edge, si.via_edge,
                                "{label}: via_edge mismatch for node {}",
                                bi.node_id
                            );
                        }
                        assert!(
                            (bp.total_weight - sp.total_weight).abs() < 1e-9,
                            "{label}: total_weight mismatch batch={} single={}",
                            bp.total_weight,
                            sp.total_weight
                        );
                    }
                    (None, Some(sp)) => {
                        panic!(
                            "{label}: batch missing path that single found ({} nodes)",
                            sp.nodes.len()
                        );
                    }
                    (Some(bp), None) => {
                        panic!(
                            "{label}: batch has path ({} nodes) that single didn't produce",
                            bp.nodes.len()
                        );
                    }
                }
            }
        }
    }

    // ── Section A: linear chains, Direction::Out ──────────────────────────────
    run_cases(
        &store,
        a,
        e,
        &[
            Case {
                include_roots: false,
                direction: Direction::Out,
                relation_filter: None,
                min_weight: None,
                limit: None,
            },
            Case {
                include_roots: true,
                direction: Direction::Out,
                relation_filter: None,
                min_weight: None,
                limit: None,
            },
            Case {
                include_roots: false,
                direction: Direction::Out,
                relation_filter: None,
                min_weight: None,
                limit: Some(1),
            },
            Case {
                include_roots: false,
                direction: Direction::Out,
                relation_filter: None,
                min_weight: None,
                limit: Some(2),
            },
            Case {
                include_roots: false,
                direction: Direction::Out,
                relation_filter: Some(EdgeRelation::Extends),
                min_weight: None,
                limit: None,
            },
            Case {
                include_roots: false,
                direction: Direction::Out,
                relation_filter: None,
                min_weight: Some(0.65),
                limit: None,
            },
        ],
        &sort_nodes,
    )
    .await;

    // ── Section B: diamond, Direction::Out ────────────────────────────────────
    // K must appear at depth=1 via the H→K shortcut edge, NOT at depth=2 via
    // H→VI→K.  A bug in the batched CTE's per-root seen-set tracking would
    // produce the wrong depth or via_edge for K.
    run_cases(
        &store,
        h,
        m,
        &[
            Case {
                include_roots: false,
                direction: Direction::Out,
                relation_filter: None,
                min_weight: None,
                limit: None,
            },
            Case {
                include_roots: true,
                direction: Direction::Out,
                relation_filter: None,
                min_weight: None,
                limit: None,
            },
        ],
        &sort_nodes,
    )
    .await;

    // ── Section C: converging node, Direction::In ─────────────────────────────
    // K has two distinct incoming edges (H→K via PartOf and VI→K via Extends).
    // Both must appear independently in K's path; neither must bleed into N's path.
    run_cases(
        &store,
        k,
        n,
        &[
            Case {
                include_roots: false,
                direction: Direction::In,
                relation_filter: None,
                min_weight: None,
                limit: None,
            },
            Case {
                include_roots: true,
                direction: Direction::In,
                relation_filter: None,
                min_weight: None,
                limit: None,
            },
        ],
        &sort_nodes,
    )
    .await;

    // ── Section D: middle node, Direction::Both ───────────────────────────────
    // VI has H→VI incoming and VI→K outgoing.  Direction::Both must walk both
    // sides and produce identical results for the batched and single-root cases.
    run_cases(
        &store,
        vi,
        n,
        &[
            Case {
                include_roots: false,
                direction: Direction::Both,
                relation_filter: None,
                min_weight: None,
                limit: None,
            },
            Case {
                include_roots: true,
                direction: Direction::Both,
                relation_filter: None,
                min_weight: None,
                limit: None,
            },
        ],
        &sort_nodes,
    )
    .await;
}

/// Regression: `limit=0` with `include_roots=false` must emit NO path for that
/// root, not an empty `GraphPath`.  Pre-fix, the Rust-level truncation reduced
/// the node list to zero but still pushed a `GraphPath { nodes: [] }`.
///
/// This test must FAIL on the commit that introduced the per-root Rust truncation
/// but BEFORE the post-truncation empty guard was added, and PASS after.
#[tokio::test]
async fn test_traverse_limit_zero_include_roots_false_emits_no_path() {
    let store = setup_memory_store();

    let root = Uuid::new_v4();
    let child = Uuid::new_v4();

    store
        .upsert_edge(make_edge(root, child, EdgeRelation::Extends, 1.0))
        .await
        .unwrap();

    let paths = store
        .traverse(TraversalRequest {
            roots: vec![root],
            options: TraversalOptions {
                max_depth: 2,
                direction: Direction::Out,
                relations: None,
                min_weight: None,
                limit: Some(0),
            },
            include_roots: false,
            include_properties: false,
        })
        .await
        .unwrap();

    assert_eq!(
        paths.len(),
        0,
        "limit=0 + include_roots=false: root has reachable children but no nodes \
         qualify under the cap, so no GraphPath should be emitted at all"
    );
}

/// Regression: traverse must not fail with "too many terms in compound SELECT"
/// when the root set is larger than CHUNK_ROOTS (400).
///
/// Pre-fix, all root UUIDs were bound into a single recursive-CTE VALUES clause.
/// SQLite's SQLITE_LIMIT_COMPOUND_SELECT (default 500) counts each VALUES row as
/// one compound-SELECT term; with 1 000 roots the query returned a StorageError
/// before the 999-variable limit was even reached.  After the fix, roots are
/// split into chunks of 400 (safely below both the 500 compound-SELECT limit and
/// the 999 variable limit), so a call with 1 000 roots is split into three chunks
/// and completes successfully.
///
/// Graph: 1 000 roots, each with one distinct outgoing edge to a unique target.
/// Correctness check: every root must appear in the result with exactly one
/// reachable node (its direct child).
#[tokio::test]
async fn traverse_chunks_root_binds_over_host_param_limit() {
    let store = setup_memory_store();

    const N: usize = 1_000;
    let mut roots: Vec<Uuid> = Vec::with_capacity(N);
    let mut expected_children: std::collections::HashMap<Uuid, Uuid> =
        std::collections::HashMap::with_capacity(N);

    for _ in 0..N {
        let root = Uuid::new_v4();
        let child = Uuid::new_v4();
        store
            .upsert_edge(make_edge(root, child, EdgeRelation::Extends, 1.0))
            .await
            .unwrap();
        roots.push(root);
        expected_children.insert(root, child);
    }

    // Must return Ok — no "too many SQL variables" error.
    let paths = store
        .traverse(TraversalRequest {
            roots: roots.clone(),
            options: TraversalOptions {
                max_depth: 1,
                direction: Direction::Out,
                relations: None,
                min_weight: None,
                limit: None,
            },
            include_roots: false,
            include_properties: false,
        })
        .await
        .unwrap();

    // Every root must have exactly one reachable node (its direct child).
    assert_eq!(
        paths.len(),
        N,
        "traverse over {N} roots must return one GraphPath per root"
    );

    for path in &paths {
        let expected_child = expected_children[&path.root_id];
        assert_eq!(
            path.nodes.len(),
            1,
            "root {:?} must reach exactly 1 node",
            path.root_id
        );
        assert_eq!(
            path.nodes[0].node_id, expected_child,
            "root {:?} must reach its direct child",
            path.root_id
        );
    }
}

/// STORAGE-AUD-003 / #485: PageRequest.offset > i64::MAX must return
/// InvalidInput instead of silently narrowing to a negative i64 offset.
#[tokio::test]
async fn page_offset_over_i64max_rejected() {
    let store = setup_memory_store();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    store
        .upsert_edge(make_edge(a, b, EdgeRelation::Extends, 1.0))
        .await
        .unwrap();

    let result = store
        .query_edges(
            EdgeFilter::default(),
            vec![],
            PageRequest {
                offset: (i64::MAX as u64) + 1,
                limit: 10,
            },
        )
        .await;

    assert!(
        matches!(result, Err(StorageError::InvalidInput { .. })),
        "expected InvalidInput, got {result:?}"
    );
}

/// STORAGE-AUD-003 / #485: TraversalOptions.max_depth > i64::MAX must return
/// InvalidInput from the backend instead of silently narrowing to a negative
/// i64 depth and returning an empty/wrong traversal.
#[tokio::test]
async fn traverse_max_depth_over_i64max_rejected() {
    let store = setup_memory_store();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    store
        .upsert_edge(make_edge(a, b, EdgeRelation::Extends, 1.0))
        .await
        .unwrap();

    let request = TraversalRequest {
        roots: vec![a],
        options: TraversalOptions::new((i64::MAX as usize) + 1).with_direction(Direction::Out),
        include_roots: false,
        include_properties: false,
    };

    let result = store.traverse(request).await;
    assert!(
        matches!(result, Err(StorageError::InvalidInput { .. })),
        "expected InvalidInput, got {result:?}"
    );
}

/// ADR-067 Component A entry 3: with `KHIVE_WRITE_QUEUE=1`, `upsert_edges`
/// routes through the WriterTask channel instead of the pool-mutex path, and
/// both edges are actually committed and independently readable back.
///
/// `#[serial]`: mutates the process-global `KHIVE_WRITE_QUEUE` env var,
/// shared with `pool.rs`'s own env-override tests in this same test binary.
#[tokio::test]
#[serial]
async fn upsert_edges_routes_through_writer_task_when_flag_enabled() {
    std::env::set_var("KHIVE_WRITE_QUEUE", "1");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write_queue_graph.db");
    let pool_cfg = PoolConfig {
        path: Some(path.clone()),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(pool_cfg).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(GRAPH_DDL).unwrap();
    }

    let store = SqlGraphStore::new_scoped(Arc::clone(&pool), true, "default");
    std::env::remove_var("KHIVE_WRITE_QUEUE");

    let e1 = make_edge(Uuid::new_v4(), Uuid::new_v4(), EdgeRelation::Extends, 0.6);
    let e2 = make_edge(Uuid::new_v4(), Uuid::new_v4(), EdgeRelation::Extends, 0.7);
    let id1 = e1.id;
    let id2 = e2.id;

    let summary = store.upsert_edges(vec![e1, e2]).await.unwrap();
    assert_eq!(summary.attempted, 2);
    assert_eq!(summary.affected, 2);
    assert_eq!(summary.failed, 0);

    assert!(store.get_edge(id1).await.unwrap().is_some());
    assert!(store.get_edge(id2).await.unwrap().is_some());
    assert_eq!(
        pool.writer_task_spawn_count(),
        1,
        "the flag-ON path must actually spawn and use the writer task"
    );
}

/// Fork C slice 2: proves the SINGLE-row `upsert_edge` (via `with_writer`,
/// distinct from the already-migrated batch `upsert_edges` above) is
/// actually enqueued on the pool's shared `WriterTaskHandle` channel when
/// `KHIVE_WRITE_QUEUE=1`.
///
/// `graph.rs`'s own flag-off/no-writer-task fallback (`open_standalone_writer`)
/// differs from entity.rs/note.rs's (`pool.try_writer()`), so a wall-clock
/// occupier-timing test is even less trustworthy here — a real file-backed
/// fallback connection opened per call would ALSO contend with the occupier
/// for SQLite's own write lock and could look "queued" by pure accident of
/// file-level locking, independent of whether `with_writer` used the shared
/// channel at all. This test sidesteps that confound entirely by reading
/// `WriterTaskHandle::queue_depth` directly — the live gauge over the exact
/// `mpsc` channel `with_writer`'s writer-task branch must call `send` on —
/// while an occupier deterministically holds the writer task's one drain
/// slot open (parked on a oneshot via `blocking_recv`, not a sleep/timing
/// race).
///
/// `#[serial]`: mutates the process-global `KHIVE_WRITE_QUEUE` env var.
#[tokio::test]
#[serial]
async fn upsert_edge_routes_through_writer_task_when_flag_enabled() {
    std::env::set_var("KHIVE_WRITE_QUEUE", "1");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write_queue_graph_single.db");
    let pool_cfg = PoolConfig {
        path: Some(path.clone()),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(pool_cfg).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(GRAPH_DDL).unwrap();
    }

    let store = Arc::new(SqlGraphStore::new_scoped(
        Arc::clone(&pool),
        true,
        "default",
    ));
    std::env::remove_var("KHIVE_WRITE_QUEUE");

    let writer_task = pool
        .writer_task_handle()
        .unwrap()
        .expect("writer task must be spawned with the flag on for a file-backed pool");

    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let occupier = {
        let writer_task = writer_task.clone();
        tokio::spawn(async move {
            writer_task
                .send(move |_conn| {
                    let _ = started_tx.send(());
                    let _ = release_rx.blocking_recv();
                    Ok::<(), StorageError>(())
                })
                .await
        })
    };

    started_rx
        .await
        .expect("occupier must signal it has started running inside the writer task");
    assert_eq!(
        writer_task.queue_depth(),
        0,
        "channel must start empty once the occupier has been dequeued and is running"
    );

    let edge = make_edge(Uuid::new_v4(), Uuid::new_v4(), EdgeRelation::Extends, 0.42);
    let edge_id = edge.id;

    let store_task = {
        let store = Arc::clone(&store);
        tokio::spawn(async move { store.upsert_edge(edge).await })
    };

    let mut saw_enqueued = false;
    for _ in 0..100 {
        if writer_task.queue_depth() >= 1 {
            saw_enqueued = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        saw_enqueued,
        "upsert_edge's write request never appeared in the writer task's \
         channel while the occupier held the single drain slot — with_writer \
         is not routing this single-row write through the shared writer task"
    );

    release_tx
        .send(())
        .expect("occupier must still be waiting on the release signal");
    occupier
        .await
        .expect("occupier task must not panic")
        .expect("occupier write must succeed");
    store_task
        .await
        .expect("store task must not panic")
        .expect("upsert_edge must succeed once unblocked");

    let fetched = store.get_edge(edge_id).await.unwrap();
    assert!(
        fetched.is_some(),
        "edge must be committed and readable after queuing behind the occupier"
    );
}
