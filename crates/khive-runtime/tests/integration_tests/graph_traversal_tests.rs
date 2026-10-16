// =============================================================================
// Graph operations
// =============================================================================

#[tokio::test]
async fn link_and_neighbors() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let lora = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "LoRA", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let qlora = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "QLoRA", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();

    rt.link(&tok, qlora.id, lora.id, EdgeRelation::VariantOf, 1.0, None)
        .await
        .unwrap();

    let hits = rt
        .neighbors(&tok, qlora.id, Direction::Out, None, None)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node_id, lora.id);
    assert_eq!(hits[0].relation, EdgeRelation::VariantOf);
}

#[tokio::test]
async fn traverse_multi_hop() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let a = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "A", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let b = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "B", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let c = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "C", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();

    rt.link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
        .await
        .unwrap();
    rt.link(&tok, b.id, c.id, EdgeRelation::Extends, 1.0, None)
        .await
        .unwrap();

    let request = TraversalRequest {
        roots: vec![a.id],
        options: TraversalOptions {
            max_depth: 2,
            direction: Direction::Out,
            relations: Some(vec![EdgeRelation::Extends]),
            ..Default::default()
        },
        include_roots: false,
        include_properties: false,
        execution_budget: Default::default(),
    };

    let paths = rt.traverse(&tok, request).await.unwrap();
    assert!(!paths.is_empty());

    // All traversed nodes should be reachable from a
    let reachable_ids: Vec<Uuid> = paths
        .iter()
        .flat_map(|p| p.nodes.iter().map(|n| n.node_id))
        .collect();
    assert!(reachable_ids.contains(&b.id));
    assert!(reachable_ids.contains(&c.id));
}

/// The soft-deleted-node screen removes nodes after storage has already
/// summarised the path, so `total_weight` must be recomputed over what
/// survives. Here the heaviest neighbour is the one soft-deleted: the
/// response must not keep reporting a weight that only a screened-out node
/// ever had.
#[tokio::test]
async fn traverse_total_weight_excludes_soft_deleted_nodes() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let a = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "A", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let light = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "light", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let heavy = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "heavy", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();

    rt.link(&tok, a.id, light.id, EdgeRelation::Extends, 0.25, None)
        .await
        .unwrap();
    rt.link(&tok, a.id, heavy.id, EdgeRelation::Extends, 0.9, None)
        .await
        .unwrap();

    let request = || TraversalRequest {
        roots: vec![a.id],
        options: TraversalOptions {
            max_depth: 1,
            direction: Direction::Out,
            relations: Some(vec![EdgeRelation::Extends]),
            ..Default::default()
        },
        include_roots: false,
        include_properties: false,
        execution_budget: Default::default(),
    };

    let before = rt.traverse(&tok, request()).await.unwrap();
    assert_eq!(before.len(), 1);
    assert_eq!(
        before[0].total_weight, 0.9,
        "baseline: the heavy neighbour sets total_weight while it is visible"
    );

    rt.delete_entity(&tok, heavy.id, false).await.unwrap(); // soft delete

    let after = rt.traverse(&tok, request()).await.unwrap();
    assert_eq!(after.len(), 1);
    let ids: Vec<Uuid> = after[0].nodes.iter().map(|n| n.node_id).collect();
    assert!(
        !ids.contains(&heavy.id),
        "soft-deleted node must be screened"
    );
    assert_eq!(
        after[0].total_weight, 0.25,
        "total_weight must fall to the surviving neighbour's weight, not stay \
         at the soft-deleted node's 0.9"
    );
}
