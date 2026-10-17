// ── #455: duplicate edge_id and symmetric duplicate identity validation ────

#[test]
fn rejects_duplicate_edge_ids_in_archive() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    let d = Uuid::new_v4();
    let dup_id = Uuid::new_v4();

    let entities = vec![
        entity(a, "A"),
        entity(b, "B"),
        entity(c, "C"),
        entity(d, "D"),
    ];
    let base = archive_full(vec![], vec![]);
    let mut ours = archive_full(entities, vec![]);
    ours.edges = vec![
        ExportedEdge {
            edge_id: dup_id,
            source: a,
            target: b,
            relation: EdgeRelation::Extends,
            weight: 1.0,
            properties: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        },
        ExportedEdge {
            edge_id: dup_id,
            source: c,
            target: d,
            relation: EdgeRelation::DependsOn,
            weight: 1.0,
            properties: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        },
    ];
    let theirs = archive_full(vec![], vec![]);

    let err = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap_err();
    match err {
        MergeError::Internal(msg) => {
            assert!(
                msg.contains("duplicate edge IDs") && msg.contains(&dup_id.to_string()),
                "expected message mentioning duplicate edge IDs and the UUID, got: {msg}"
            );
        }
        other => panic!("expected MergeError::Internal, got: {other:?}"),
    }
}

#[test]
fn rejects_swapped_symmetric_duplicate_edges_in_archive() {
    let (lo, hi) = {
        let x = Uuid::new_v4();
        let y = Uuid::new_v4();
        if x < y {
            (x, y)
        } else {
            (y, x)
        }
    };

    let entities = vec![entity(lo, "A"), entity(hi, "B")];
    let base = archive_full(vec![], vec![]);
    let ours = archive_full(
        entities,
        vec![
            ExportedEdge {
                edge_id: Uuid::new_v4(),
                source: lo,
                target: hi,
                relation: EdgeRelation::CompetesWith,
                weight: 1.0,
                properties: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
            ExportedEdge {
                edge_id: Uuid::new_v4(),
                source: hi,
                target: lo,
                relation: EdgeRelation::CompetesWith,
                weight: 1.0,
                properties: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
        ],
    );
    let theirs = archive_full(vec![], vec![]);

    let err = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap_err();
    match err {
        MergeError::DuplicateEdgeKey {
            edge_source,
            edge_target,
            edge_relation,
        } => {
            assert_eq!(
                edge_source, lo,
                "canonical source must be min(source, target)"
            );
            assert_eq!(
                edge_target, hi,
                "canonical target must be max(source, target)"
            );
            assert_eq!(edge_relation, "competes_with");
        }
        other => panic!("expected MergeError::DuplicateEdgeKey, got: {other:?}"),
    }
}

#[test]
fn modified_symmetric_edge_is_emitted_with_canonical_endpoints() {
    let (lo, hi) = {
        let x = Uuid::new_v4();
        let y = Uuid::new_v4();
        if x < y {
            (x, y)
        } else {
            (y, x)
        }
    };

    let base_edge = ExportedEdge {
        edge_id: Uuid::new_v4(),
        source: lo,
        target: hi,
        relation: EdgeRelation::CompetesWith,
        weight: 0.5,
        properties: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    // Theirs modifies the weight and carries the endpoints reversed; the
    // semantic key matches base, so this is a modification, not an add.
    let mut theirs_edge = base_edge.clone();
    theirs_edge.source = hi;
    theirs_edge.target = lo;
    theirs_edge.weight = 0.9;

    let entities = vec![entity(lo, "A"), entity(hi, "B")];
    let base = archive_full(entities.clone(), vec![base_edge.clone()]);
    let ours = archive_full(entities.clone(), vec![base_edge]);
    let theirs = archive_full(entities, vec![theirs_edge]);

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.edges.len(), 1);
        assert_eq!(merged.edges[0].weight, 0.9);
        assert_eq!(
            merged.edges[0].source, lo,
            "canonical source must be min(source, target)"
        );
        assert_eq!(
            merged.edges[0].target, hi,
            "canonical target must be max(source, target)"
        );
    } else {
        panic!("expected Clean, got: {result:?}");
    }
}

// ── #456: shortcut strategies must not report dangling edges as Clean ──────

#[test]
fn ours_strategy_reports_dangling_edge_after_shortcut() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();

    let base = archive_full(vec![entity(a, "A")], vec![]);
    // ours deletes A and has no edges.
    let ours = archive_full(vec![], vec![]);
    // theirs retains A, adds B, and adds edge B -> A.
    let theirs = archive_full(vec![entity(a, "A"), entity(b, "B")], vec![edge(b, a)]);

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Ours).unwrap();
    assert!(
        matches!(result, MergeResult::Conflicts { .. }),
        "shortcut merge with a dangling edge must not be Clean"
    );
    if let MergeResult::Conflicts { conflicts } = result {
        assert!(
            conflicts.iter().any(
                |c| matches!(c, MergeConflict::DanglingEdge { missing_endpoint, .. } if *missing_endpoint == a)
            ),
            "expected DanglingEdge for missing entity A, got: {conflicts:?}"
        );
    }
}

#[test]
fn theirs_strategy_reports_dangling_edge_after_shortcut() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();

    let base = archive_full(vec![entity(a, "A")], vec![]);
    // theirs deletes A and has no edges.
    let theirs = archive_full(vec![], vec![]);
    // ours retains A, adds B, and adds edge B -> A.
    let ours = archive_full(vec![entity(a, "A"), entity(b, "B")], vec![edge(b, a)]);

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Theirs).unwrap();
    assert!(
        matches!(result, MergeResult::Conflicts { .. }),
        "shortcut merge with a dangling edge must not be Clean"
    );
    if let MergeResult::Conflicts { conflicts } = result {
        assert!(
            conflicts.iter().any(
                |c| matches!(c, MergeConflict::DanglingEdge { missing_endpoint, .. } if *missing_endpoint == a)
            ),
            "expected DanglingEdge for missing entity A, got: {conflicts:?}"
        );
    }
}

#[test]
fn ours_strategy_reports_retargeted_edge_identity_collision() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    let entities = vec![entity(a, "A"), entity(b, "B"), entity(c, "C")];
    let preferred = edge(a, b);
    let mut retargeted = preferred.clone();
    retargeted.target = c;
    let base = archive_full(entities.clone(), vec![preferred.clone()]);
    let ours = archive_full(entities.clone(), vec![preferred.clone()]);
    let theirs = archive_full(entities, vec![retargeted]);

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Ours).unwrap();
    assert!(matches!(
        result,
        MergeResult::Conflicts { conflicts }
            if conflicts.iter().any(|conflict| matches!(
                conflict,
                MergeConflict::EdgeIdentityCollision {
                    target_id,
                    attempted_edge_id,
                    retained_edge_id,
                    ..
                } if *target_id == c
                    && *attempted_edge_id == preferred.edge_id
                    && *retained_edge_id == preferred.edge_id
            ))
    ));
}

#[test]
fn theirs_strategy_reports_retargeted_edge_identity_collision() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    let entities = vec![entity(a, "A"), entity(b, "B"), entity(c, "C")];
    let preferred = edge(a, b);
    let mut retargeted = preferred.clone();
    retargeted.target = c;
    let base = archive_full(entities.clone(), vec![preferred.clone()]);
    let ours = archive_full(entities.clone(), vec![retargeted]);
    let theirs = archive_full(entities, vec![preferred.clone()]);

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Theirs).unwrap();
    assert!(matches!(
        result,
        MergeResult::Conflicts { conflicts }
            if conflicts.iter().any(|conflict| matches!(
                conflict,
                MergeConflict::EdgeIdentityCollision {
                    target_id,
                    attempted_edge_id,
                    retained_edge_id,
                    ..
                } if *target_id == c
                    && *attempted_edge_id == preferred.edge_id
                    && *retained_edge_id == preferred.edge_id
            ))
    ));
}
