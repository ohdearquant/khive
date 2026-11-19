#[test]
fn auto_reports_divergent_edge_property_changes() {
    let (base, ours, theirs) = divergent_property_archives();
    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();

    assert!(matches!(
        result,
        MergeResult::Conflicts { ref conflicts }
            if conflicts.iter().any(|conflict| matches!(
                conflict,
                MergeConflict::EdgePropertyMismatch { .. }
            ))
    ));
}

#[test]
fn ours_strategy_resolves_divergent_edge_properties_to_ours() {
    let (base, ours, theirs) = divergent_property_archives();
    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Ours).unwrap();

    match result {
        MergeResult::Clean { merged } => {
            assert_eq!(
                merged.edges[0].properties,
                Some(serde_json::json!({"confidence": 0.8}))
            );
            assert_eq!(
                merged.edges[0].created_at.to_rfc3339(),
                "2026-05-05T00:00:00+00:00"
            );
            assert_eq!(
                merged.edges[0].updated_at.to_rfc3339(),
                "2026-06-06T00:00:00+00:00"
            );
        }
        MergeResult::Conflicts { conflicts } => {
            panic!("ours strategy must resolve the property conflict: {conflicts:?}")
        }
    }
}

#[test]
fn theirs_strategy_resolves_divergent_edge_properties_to_theirs() {
    let (base, ours, theirs) = divergent_property_archives();
    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Theirs).unwrap();

    match result {
        MergeResult::Clean { merged } => {
            assert_eq!(
                merged.edges[0].properties,
                Some(serde_json::json!({"confidence": 0.9}))
            );
            assert_eq!(
                merged.edges[0].created_at.to_rfc3339(),
                "2026-07-07T00:00:00+00:00"
            );
            assert_eq!(
                merged.edges[0].updated_at.to_rfc3339(),
                "2026-08-08T00:00:00+00:00"
            );
        }
        MergeResult::Conflicts { conflicts } => {
            panic!("theirs strategy must resolve the property conflict: {conflicts:?}")
        }
    }
}

#[test]
fn merge_preserves_weight_modified_edge_metadata_and_provenance() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let created = Utc::now() - chrono::Duration::days(30);
    let props = serde_json::json!({"origin": "import", "confidence": 0.7});

    let base_edge = ExportedEdge {
        edge_id: Uuid::new_v4(),
        source: a,
        target: b,
        relation: EdgeRelation::Extends,
        weight: 0.5,
        properties: Some(props.clone()),
        created_at: created,
        updated_at: created,
    };
    let mut ours_edge = base_edge.clone();
    ours_edge.weight = 0.9;

    let entities = vec![entity(a, "A"), entity(b, "B")];
    let base = archive_full(entities.clone(), vec![base_edge.clone()]);
    let ours = archive_full(entities.clone(), vec![ours_edge]);
    let theirs = archive_full(entities, vec![base_edge]);

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.edges.len(), 1);
        assert_eq!(merged.edges[0].weight, 0.9);
        assert_eq!(
            merged.edges[0].properties,
            Some(props),
            "weight merge must carry the surviving record's metadata"
        );
        assert_eq!(
            merged.edges[0].created_at, created,
            "weight merge must not fabricate provenance timestamps"
        );
    } else {
        panic!("expected Clean");
    }
}
