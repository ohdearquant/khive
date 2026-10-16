// ── Edge merge scenarios ────────────────────────────────────────────────────

#[test]
fn edge_added_in_ours_included() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let entities = vec![entity(a, "A"), entity(b, "B")];
    let base = archive_full(entities.clone(), vec![]);
    let ours = archive_full(entities.clone(), vec![edge(a, b)]);
    let theirs = archive_full(entities, vec![]);
    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.edges.len(), 1);
    } else {
        panic!("expected Clean");
    }
}

#[test]
fn edge_deleted_in_both_excluded() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let entities = vec![entity(a, "A"), entity(b, "B")];
    let base = archive_full(entities.clone(), vec![edge(a, b)]);
    let ours = archive_full(entities.clone(), vec![]);
    let theirs = archive_full(entities, vec![]);
    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.edges.len(), 0);
    } else {
        panic!("expected Clean");
    }
}

#[test]
fn max_weight_on_both_added() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let entities = vec![entity(a, "A"), entity(b, "B")];
    let base = archive_full(entities.clone(), vec![]);
    let ours = archive_full(entities.clone(), vec![edge_weighted(a, b, 0.6)]);
    let theirs = archive_full(entities, vec![edge_weighted(a, b, 0.9)]);
    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.edges.len(), 1);
        assert!((merged.edges[0].weight - 0.9).abs() < f64::EPSILON);
    } else {
        panic!("expected Clean");
    }
}

#[test]
fn dangling_edge_detected() {
    use std::collections::HashSet;
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let edges = vec![edge(a, b)];
    let entity_ids: HashSet<Uuid> = [a].into_iter().collect();
    let conflicts = khive_merge::edge::validate_dangling_edges(&edges, &entity_ids);
    assert_eq!(conflicts.len(), 1);
    assert!(
        matches!(&conflicts[0], MergeConflict::DanglingEdge { missing_endpoint, .. } if *missing_endpoint == b)
    );
}

#[test]
fn edge_modify_delete_conflict() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let entities = vec![entity(a, "A"), entity(b, "B")];
    let base = archive_full(entities.clone(), vec![edge_weighted(a, b, 0.5)]);
    let ours = archive_full(entities.clone(), vec![]);
    let theirs = archive_full(entities, vec![edge_weighted(a, b, 1.0)]);

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    assert!(matches!(result, MergeResult::Conflicts { .. }));
    if let MergeResult::Conflicts { conflicts } = result {
        assert!(
            conflicts
                .iter()
                .any(|c| matches!(c, MergeConflict::EdgeModifyDelete { .. })),
            "expected EdgeModifyDelete conflict, got: {conflicts:?}"
        );
    }
}

#[test]
fn merge_preserves_added_edge_id() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let branch_edge = edge(a, b);
    let expected_id = branch_edge.edge_id;
    let entities = vec![entity(a, "A"), entity(b, "B")];

    let base = archive_full(entities.clone(), vec![]);
    let ours = archive_full(entities.clone(), vec![branch_edge]);
    let theirs = archive_full(entities, vec![]);

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.edges.len(), 1);
        assert_eq!(
            merged.edges[0].edge_id, expected_id,
            "merged edge_id must equal the branch's edge_id"
        );
    } else {
        panic!("expected Clean");
    }
}

#[test]
fn merge_preserves_weight_modified_edge_id() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();

    let base_edge = ExportedEdge {
        edge_id: Uuid::new_v4(),
        source: a,
        target: b,
        relation: EdgeRelation::Extends,
        weight: 0.5,
        properties: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let ours_edge = ExportedEdge {
        edge_id: Uuid::new_v4(),
        source: a,
        target: b,
        relation: EdgeRelation::Extends,
        weight: 0.9,
        properties: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let expected_id = ours_edge.edge_id;
    let entities = vec![entity(a, "A"), entity(b, "B")];

    let base = archive_full(entities.clone(), vec![base_edge.clone()]);
    let ours = archive_full(entities.clone(), vec![ours_edge]);
    let theirs = archive_full(entities, vec![base_edge]);

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.edges.len(), 1);
        assert_eq!(merged.edges[0].weight, 0.9);
        assert_eq!(
            merged.edges[0].edge_id, expected_id,
            "merged edge_id must equal ours' edge_id after weight modification"
        );
    } else {
        panic!("expected Clean");
    }
}

#[test]
fn auto_preserves_one_sided_property_only_edge_with_provenance() {
    let a = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
    let b = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();
    let entities = vec![entity(a, "A"), entity(b, "B")];
    let mut base_edge = edge_weighted(a, b, 0.7);
    base_edge.created_at = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    base_edge.updated_at = chrono::DateTime::parse_from_rfc3339("2026-01-02T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut ours_edge = base_edge.clone();
    ours_edge.properties = Some(serde_json::json!({"confidence": 0.95}));
    ours_edge.created_at = chrono::DateTime::parse_from_rfc3339("2026-03-03T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    ours_edge.updated_at = chrono::DateTime::parse_from_rfc3339("2026-04-04T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    let base = archive_full(entities.clone(), vec![base_edge.clone()]);
    let ours = archive_full(entities.clone(), vec![ours_edge.clone()]);
    let theirs = archive_full(entities, vec![base_edge]);
    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();

    match result {
        MergeResult::Clean { merged } => {
            assert_eq!(merged.edges.len(), 1);
            assert_eq!(merged.edges[0].edge_id, ours_edge.edge_id);
            assert_eq!(merged.edges[0].properties, ours_edge.properties);
            assert_eq!(merged.edges[0].created_at, ours_edge.created_at);
            assert_eq!(merged.edges[0].updated_at, ours_edge.updated_at);
        }
        MergeResult::Conflicts { conflicts } => {
            panic!("one-sided property change must merge cleanly: {conflicts:?}")
        }
    }
}
