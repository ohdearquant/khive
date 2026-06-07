use super::*;
use crate::pool::PoolConfig;
use khive_storage::types::{Direction, TraversalOptions};

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
