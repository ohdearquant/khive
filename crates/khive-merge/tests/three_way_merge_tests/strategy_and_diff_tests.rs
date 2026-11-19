// ── Strategy tests ──────────────────────────────────────────────────────────

#[test]
fn apply_ours_uses_ours_version() {
    let id = Uuid::new_v4();
    let mut base = empty("test");
    base.entities = vec![entity(id, "Original")];
    let mut ours = empty("test");
    ours.entities = vec![entity(id, "OursName")];
    let mut theirs = empty("test");
    theirs.entities = vec![entity(id, "TheirsName")];

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Ours).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.entities.len(), 1);
        assert_eq!(merged.entities[0].name, "OursName");
    } else {
        panic!("expected Clean");
    }
}

#[test]
fn apply_theirs_uses_theirs_version() {
    let id = Uuid::new_v4();
    let mut base = empty("test");
    base.entities = vec![entity(id, "Original")];
    let mut ours = empty("test");
    ours.entities = vec![entity(id, "OursName")];
    let mut theirs = empty("test");
    theirs.entities = vec![entity(id, "TheirsName")];

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Theirs).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.entities.len(), 1);
        assert_eq!(merged.entities[0].name, "TheirsName");
    } else {
        panic!("expected Clean");
    }
}

#[test]
fn apply_ours_includes_theirs_only_additions() {
    let id_ours = Uuid::new_v4();
    let id_theirs = Uuid::new_v4();
    let base = empty("test");
    let mut ours = empty("test");
    ours.entities = vec![entity(id_ours, "A")];
    let mut theirs = empty("test");
    theirs.entities = vec![entity(id_theirs, "B")];

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Ours).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.entities.len(), 2);
    } else {
        panic!("expected Clean");
    }
}

// ── Diff tests ──────────────────────────────────────────────────────────────

#[test]
fn diff_unchanged_entity() {
    use khive_merge::diff_local::{diff_entities, EntityChange};
    let id = Uuid::new_v4();
    let e = entity(id, "FlashAttention");
    let base = archive_with_entities(vec![e.clone()]);
    let branch = archive_with_entities(vec![e]);
    let diff = diff_entities(&base, &branch);
    assert!(matches!(diff[&id], EntityChange::Unchanged));
}

#[test]
fn diff_added_entity() {
    use khive_merge::diff_local::{diff_entities, EntityChange};
    let id = Uuid::new_v4();
    let base = archive_with_entities(vec![]);
    let branch = archive_with_entities(vec![entity(id, "New")]);
    let diff = diff_entities(&base, &branch);
    assert!(matches!(diff[&id], EntityChange::Added(_)));
}

#[test]
fn diff_deleted_entity() {
    use khive_merge::diff_local::{diff_entities, EntityChange};
    let id = Uuid::new_v4();
    let base = archive_with_entities(vec![entity(id, "Old")]);
    let branch = archive_with_entities(vec![]);
    let diff = diff_entities(&base, &branch);
    assert!(matches!(diff[&id], EntityChange::Deleted));
}

#[test]
fn diff_modified_entity_name() {
    use khive_merge::diff_local::{diff_entities, EntityChange};
    let id = Uuid::new_v4();
    let base = archive_with_entities(vec![entity(id, "Original")]);
    let mut e2 = entity(id, "Original");
    e2.name = "Renamed".into();
    let branch = archive_with_entities(vec![e2]);
    let diff = diff_entities(&base, &branch);
    assert!(matches!(diff[&id], EntityChange::Modified { .. }));
}

#[test]
fn diff_unchanged_edge() {
    use khive_merge::diff_local::{diff_edges, EdgeChange, EdgeKey};
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let e = edge(a, b);
    let base = archive_with_edges(vec![e.clone()]);
    let branch = archive_with_edges(vec![e]);
    let diff = diff_edges(&base, &branch).unwrap();
    let key = EdgeKey {
        source: a,
        target: b,
        relation: "extends".into(),
    };
    assert!(matches!(diff[&key], EdgeChange::Unchanged));
}

#[test]
fn diff_added_edge() {
    use khive_merge::diff_local::{diff_edges, EdgeChange, EdgeKey};
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let base = archive_with_edges(vec![]);
    let branch = archive_with_edges(vec![edge_weighted(a, b, 0.8)]);
    let diff = diff_edges(&base, &branch).unwrap();
    let key = EdgeKey {
        source: a,
        target: b,
        relation: "extends".into(),
    };
    assert!(matches!(diff[&key], EdgeChange::Added(_)));
}

#[test]
fn diff_weight_change_is_modified_edge() {
    use khive_merge::diff_local::{diff_edges, EdgeChange, EdgeKey};
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let base = archive_with_edges(vec![edge_weighted(a, b, 0.5)]);
    let branch = archive_with_edges(vec![edge_weighted(a, b, 1.0)]);
    let diff = diff_edges(&base, &branch).unwrap();
    let key = EdgeKey {
        source: a,
        target: b,
        relation: "extends".into(),
    };
    assert!(matches!(diff[&key], EdgeChange::Modified { .. }));
}

// ── #454: entity_type must participate in diff and merge ───────────────────

#[test]
fn diff_entity_type_change_is_modified() {
    use khive_merge::diff_local::{diff_entities, EntityChange};
    let id = Uuid::new_v4();
    let base = archive_with_entities(vec![entity_with_type(id, "FlashAttention", None)]);
    let branch = archive_with_entities(vec![entity_with_type(
        id,
        "FlashAttention",
        Some("algorithm"),
    )]);
    let diff = diff_entities(&base, &branch);
    assert!(
        matches!(diff[&id], EntityChange::Modified { .. }),
        "entity_type-only change must be classified Modified, got: {:?}",
        diff[&id]
    );
}

#[test]
fn auto_merge_preserves_single_sided_entity_type_change() {
    let id = Uuid::new_v4();
    let base = archive_with_entities(vec![entity_with_type(id, "U", None)]);
    let ours = archive_with_entities(vec![entity_with_type(id, "U", Some("algorithm"))]);
    let theirs = archive_with_entities(vec![entity_with_type(id, "U", None)]);

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(
            merged.entities[0].entity_type,
            Some("algorithm".to_string()),
            "single-sided entity_type change must survive the merge"
        );
    } else {
        panic!("expected Clean, got Conflicts");
    }
}

#[test]
fn auto_merge_conflicts_on_divergent_entity_type_changes() {
    let id = Uuid::new_v4();
    let base = archive_with_entities(vec![entity_with_type(id, "U", None)]);
    let ours = archive_with_entities(vec![entity_with_type(id, "U", Some("algorithm"))]);
    let theirs = archive_with_entities(vec![entity_with_type(id, "U", Some("technique"))]);

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    assert!(matches!(result, MergeResult::Conflicts { .. }));
    if let MergeResult::Conflicts { conflicts } = result {
        assert!(
            conflicts.iter().any(
                |c| matches!(c, MergeConflict::PropertyMismatch { key, .. } if key == "entity_type")
            ),
            "expected PropertyMismatch{{key: \"entity_type\"}}, got: {conflicts:?}"
        );
    }
}

#[test]
fn duplicate_uuid_different_entity_type_is_conflict() {
    let id = Uuid::new_v4();
    let base = archive_with_entities(vec![]);
    let ours = archive_with_entities(vec![entity_with_type(id, "U", Some("algorithm"))]);
    let theirs = archive_with_entities(vec![entity_with_type(id, "U", Some("technique"))]);

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    assert!(matches!(result, MergeResult::Conflicts { .. }));
    if let MergeResult::Conflicts { conflicts } = result {
        assert!(
            conflicts.iter().any(|c| matches!(
                c,
                MergeConflict::DuplicateAddition { differing_fields, .. }
                    if differing_fields.contains(&"entity_type".to_string())
            )),
            "expected DuplicateAddition with entity_type in differing_fields, got: {conflicts:?}"
        );
    }
}
