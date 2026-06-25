use super::*;
use crate::pool::PoolConfig;
use khive_storage::types::{Direction, TraversalOptions};
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

#[tokio::test]
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
#[tokio::test]
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
#[tokio::test]
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
#[tokio::test]
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
#[tokio::test]
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
#[tokio::test]
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
#[tokio::test]
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
#[tokio::test]
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
