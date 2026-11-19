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
