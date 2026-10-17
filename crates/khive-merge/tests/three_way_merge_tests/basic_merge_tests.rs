// ── Basic merge scenarios ───────────────────────────────────────────────────

#[test]
fn clean_merge_no_overlap() {
    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();
    let base = empty("test");
    let mut ours = empty("test");
    ours.entities = vec![entity(id1, "A")];
    let mut theirs = empty("test");
    theirs.entities = vec![entity(id2, "B")];

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    assert!(matches!(result, MergeResult::Clean { .. }));
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.entities.len(), 2);
    }
}

#[test]
fn conflicts_on_name_mismatch() {
    let id = Uuid::new_v4();
    let base = {
        let mut a = empty("test");
        a.entities = vec![entity(id, "Original")];
        a
    };
    let ours = {
        let mut a = empty("test");
        a.entities = vec![entity(id, "NameA")];
        a
    };
    let theirs = {
        let mut a = empty("test");
        a.entities = vec![entity(id, "NameB")];
        a
    };

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    assert!(matches!(result, MergeResult::Conflicts { .. }));
}

#[test]
fn ours_strategy_always_clean() {
    let id = Uuid::new_v4();
    let base = {
        let mut a = empty("test");
        a.entities = vec![entity(id, "Original")];
        a
    };
    let ours = {
        let mut a = empty("test");
        a.entities = vec![entity(id, "NameA")];
        a
    };
    let theirs = {
        let mut a = empty("test");
        a.entities = vec![entity(id, "NameB")];
        a
    };

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Ours).unwrap();
    assert!(matches!(result, MergeResult::Clean { .. }));
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.entities[0].name, "NameA");
    }
}

#[test]
fn theirs_strategy_always_clean() {
    let id = Uuid::new_v4();
    let base = {
        let mut a = empty("test");
        a.entities = vec![entity(id, "Original")];
        a
    };
    let ours = {
        let mut a = empty("test");
        a.entities = vec![entity(id, "NameA")];
        a
    };
    let theirs = {
        let mut a = empty("test");
        a.entities = vec![entity(id, "NameB")];
        a
    };

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Theirs).unwrap();
    assert!(matches!(result, MergeResult::Clean { .. }));
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.entities[0].name, "NameB");
    }
}

#[test]
fn dangling_edge_in_auto_merge() {
    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();

    let base = {
        let mut a = empty("test");
        a.entities = vec![entity(id1, "A")];
        a
    };
    let ours = {
        let mut a = empty("test");
        a.entities = vec![entity(id1, "A"), entity(id2, "B")];
        a.edges = vec![edge(id1, id2)];
        a
    };
    let theirs = {
        let mut a = empty("test");
        a.entities = vec![entity(id1, "A")];
        a
    };

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    assert!(matches!(result, MergeResult::Clean { .. }));
}

#[test]
fn three_way_merge_engine_impl() {
    let engine = ThreeWayMergeEngine;
    let base = empty("test");
    let ours = empty("test");
    let theirs = empty("test");
    let result = engine
        .merge_branch(&base, &ours, &theirs, SnapshotMergeStrategy::Auto)
        .unwrap();
    assert!(matches!(result, MergeResult::Clean { .. }));
}

#[test]
fn kind_conflict_detected() {
    let id = Uuid::new_v4();
    let base = {
        let mut a = empty("test");
        a.entities = vec![entity(id, "E")];
        a
    };
    let ours = {
        let mut a = empty("test");
        let mut e = entity(id, "E");
        e.kind = "document".into();
        a.entities = vec![e];
        a
    };
    let theirs = {
        let mut a = empty("test");
        let mut e = entity(id, "E");
        e.kind = "dataset".into();
        a.entities = vec![e];
        a
    };

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    assert!(matches!(result, MergeResult::Conflicts { .. }));
    if let MergeResult::Conflicts { conflicts } = result {
        assert!(
            conflicts
                .iter()
                .any(|c| matches!(c, MergeConflict::KindConflict { .. })),
            "expected at least one KindConflict, got: {conflicts:?}"
        );
    }
}
