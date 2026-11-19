// ── Entity merge scenarios ──────────────────────────────────────────────────

#[test]
fn unchanged_entity_passes_through() {
    let id = Uuid::new_v4();
    let e = entity(id, "A");
    let base = archive_with_entities(vec![e.clone()]);
    let ours = archive_with_entities(vec![e.clone()]);
    let theirs = archive_with_entities(vec![e]);
    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.entities.len(), 1);
        assert_eq!(merged.entities[0].name, "A");
    } else {
        panic!("expected Clean");
    }
}

#[test]
fn added_in_ours_included() {
    let id = Uuid::new_v4();
    let base = archive_with_entities(vec![]);
    let ours = archive_with_entities(vec![entity(id, "New")]);
    let theirs = archive_with_entities(vec![]);
    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.entities.len(), 1);
    } else {
        panic!("expected Clean");
    }
}

#[test]
fn deleted_in_both_excluded() {
    let id = Uuid::new_v4();
    let base = archive_with_entities(vec![entity(id, "Old")]);
    let ours = archive_with_entities(vec![]);
    let theirs = archive_with_entities(vec![]);
    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.entities.len(), 0);
    } else {
        panic!("expected Clean");
    }
}

#[test]
fn modify_delete_conflict() {
    let id = Uuid::new_v4();
    let mut modified = entity(id, "Original");
    let base = archive_with_entities(vec![entity(id, "Original")]);
    modified.name = "Renamed".into();
    let ours = archive_with_entities(vec![modified]);
    let theirs = archive_with_entities(vec![]);

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    assert!(matches!(result, MergeResult::Conflicts { .. }));
    if let MergeResult::Conflicts { conflicts } = result {
        assert_eq!(conflicts.len(), 1);
        assert!(matches!(conflicts[0], MergeConflict::ModifyDelete { .. }));
    }
}

#[test]
fn property_mismatch_conflict() {
    let id = Uuid::new_v4();
    let mut e_ours = entity(id, "E");
    let mut e_theirs = entity(id, "E");
    e_ours.properties = Some(serde_json::json!({"year": "2023"}));
    e_theirs.properties = Some(serde_json::json!({"year": "2022"}));

    let base = archive_with_entities(vec![entity(id, "E")]);
    let ours = archive_with_entities(vec![e_ours]);
    let theirs = archive_with_entities(vec![e_theirs]);

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    assert!(matches!(result, MergeResult::Conflicts { .. }));
    if let MergeResult::Conflicts { conflicts } = result {
        assert!(conflicts
            .iter()
            .any(|c| matches!(c, MergeConflict::PropertyMismatch { .. })));
    }
}

#[test]
fn name_conflict_reported() {
    let id = Uuid::new_v4();
    let mut e_ours = entity(id, "OriginalName");
    let mut e_theirs = entity(id, "OriginalName");
    let base = archive_with_entities(vec![entity(id, "OriginalName")]);
    e_ours.name = "NameA".into();
    e_theirs.name = "NameB".into();

    let ours = archive_with_entities(vec![e_ours]);
    let theirs = archive_with_entities(vec![e_theirs]);

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    assert!(matches!(result, MergeResult::Conflicts { .. }));
    if let MergeResult::Conflicts { conflicts } = result {
        assert!(conflicts
            .iter()
            .any(|c| matches!(c, MergeConflict::NameConflict { .. })));
    }
}

#[test]
fn tags_are_unioned() {
    let id = Uuid::new_v4();
    let mut e_ours = entity(id, "E");
    let mut e_theirs = entity(id, "E");
    let base = archive_with_entities(vec![entity(id, "E")]);
    e_ours.tags = vec!["a".into(), "b".into()];
    e_theirs.tags = vec!["b".into(), "c".into()];

    let ours = archive_with_entities(vec![e_ours]);
    let theirs = archive_with_entities(vec![e_theirs]);

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        let tags = &merged.entities[0].tags;
        assert!(tags.contains(&"a".to_string()));
        assert!(tags.contains(&"b".to_string()));
        assert!(tags.contains(&"c".to_string()));
    } else {
        panic!("expected Clean (tag conflicts not expected)");
    }
}

#[test]
fn theirs_only_property_keys_preserved() {
    let id = Uuid::new_v4();
    let mut e_ours = entity(id, "E");
    let mut e_theirs = entity(id, "E");
    let base = archive_with_entities(vec![entity(id, "E")]);
    e_ours.properties = Some(serde_json::json!({"year": "2023"}));
    e_theirs.properties = Some(serde_json::json!({"year": "2023", "author": "Smith"}));

    let ours = archive_with_entities(vec![e_ours]);
    let theirs = archive_with_entities(vec![e_theirs]);

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        let props = merged.entities[0]
            .properties
            .as_ref()
            .expect("merged has properties");
        assert_eq!(props.get("year").and_then(|v| v.as_str()), Some("2023"));
        assert_eq!(
            props.get("author").and_then(|v| v.as_str()),
            Some("Smith"),
            "theirs-only key 'author' must be preserved in merged output"
        );
    } else {
        panic!("expected Clean");
    }
}
