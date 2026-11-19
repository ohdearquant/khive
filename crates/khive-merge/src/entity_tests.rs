use chrono::Utc;
use khive_runtime::portability::{ExportedEntity, KgArchive};
use uuid::Uuid;

use super::*;

fn archive_with(entities: Vec<ExportedEntity>) -> KgArchive {
    KgArchive {
        format: "khive-kg".into(),
        version: "0.1".into(),
        namespace: "test".into(),
        exported_at: Utc::now(),
        entities,
        edges: vec![],
    }
}

fn entity(id: Uuid, name: &str) -> ExportedEntity {
    ExportedEntity {
        id,
        kind: "concept".into(),
        name: name.into(),
        description: None,
        properties: None,
        tags: vec![],
        created_at: Utc::now(),
        updated_at: Utc::now(),
        entity_type: None,
    }
}

#[test]
fn unchanged_entity_passes_through() {
    let id = Uuid::new_v4();
    let e = entity(id, "A");
    let base = archive_with(vec![e.clone()]);
    let ours = archive_with(vec![e.clone()]);
    let theirs = archive_with(vec![e]);
    let (merged, conflicts) = merge_entities(&base, &ours, &theirs);
    assert!(conflicts.is_empty());
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].name, "A");
}

#[test]
fn added_in_ours_included() {
    let id = Uuid::new_v4();
    let base = archive_with(vec![]);
    let ours = archive_with(vec![entity(id, "New")]);
    let theirs = archive_with(vec![]);
    let (merged, conflicts) = merge_entities(&base, &ours, &theirs);
    assert!(conflicts.is_empty());
    assert_eq!(merged.len(), 1);
}

#[test]
fn deleted_in_both_excluded() {
    let id = Uuid::new_v4();
    let base = archive_with(vec![entity(id, "Old")]);
    let ours = archive_with(vec![]);
    let theirs = archive_with(vec![]);
    let (merged, conflicts) = merge_entities(&base, &ours, &theirs);
    assert!(conflicts.is_empty());
    assert_eq!(merged.len(), 0);
}

#[test]
fn modify_delete_conflict() {
    let id = Uuid::new_v4();
    let mut modified = entity(id, "Original");
    let base = archive_with(vec![entity(id, "Original")]);
    modified.name = "Renamed".into();
    let ours = archive_with(vec![modified]);
    let theirs = archive_with(vec![]);

    let (_, conflicts) = merge_entities(&base, &ours, &theirs);
    assert_eq!(conflicts.len(), 1);
    assert!(matches!(conflicts[0], MergeConflict::ModifyDelete { .. }));
}

#[test]
fn property_mismatch_conflict() {
    let id = Uuid::new_v4();
    let mut e_ours = entity(id, "E");
    let mut e_theirs = entity(id, "E");
    e_ours.properties = Some(serde_json::json!({"year": "2023"}));
    e_theirs.properties = Some(serde_json::json!({"year": "2022"}));

    let base = archive_with(vec![entity(id, "E")]);
    let ours = archive_with(vec![e_ours]);
    let theirs = archive_with(vec![e_theirs]);

    let (_, conflicts) = merge_entities(&base, &ours, &theirs);
    assert!(!conflicts.is_empty());
    assert!(matches!(
        conflicts[0],
        MergeConflict::PropertyMismatch { .. }
    ));
}

#[test]
fn name_conflict_reported() {
    let id = Uuid::new_v4();
    let mut e_ours = entity(id, "OriginalName");
    let mut e_theirs = entity(id, "OriginalName");
    let base = archive_with(vec![entity(id, "OriginalName")]);
    e_ours.name = "NameA".into();
    e_theirs.name = "NameB".into();

    let ours = archive_with(vec![e_ours]);
    let theirs = archive_with(vec![e_theirs]);

    let (_, conflicts) = merge_entities(&base, &ours, &theirs);
    assert!(conflicts
        .iter()
        .any(|c| matches!(c, MergeConflict::NameConflict { .. })));
}

#[test]
fn tags_are_unioned() {
    let id = Uuid::new_v4();
    let mut e_ours = entity(id, "E");
    let mut e_theirs = entity(id, "E");
    let base = archive_with(vec![entity(id, "E")]);
    e_ours.tags = vec!["a".into(), "b".into()];
    e_theirs.tags = vec!["b".into(), "c".into()];

    let ours = archive_with(vec![e_ours]);
    let theirs = archive_with(vec![e_theirs]);

    let (merged, _) = merge_entities(&base, &ours, &theirs);
    let tags = &merged[0].tags;
    assert!(tags.contains(&"a".to_string()));
    assert!(tags.contains(&"b".to_string()));
    assert!(tags.contains(&"c".to_string()));
}

#[test]
fn theirs_only_property_keys_preserved() {
    let id = Uuid::new_v4();
    let mut e_ours = entity(id, "E");
    let mut e_theirs = entity(id, "E");
    let base = archive_with(vec![entity(id, "E")]);
    e_ours.properties = Some(serde_json::json!({"year": "2023"}));
    e_theirs.properties = Some(serde_json::json!({"year": "2023", "author": "Smith"}));

    let ours = archive_with(vec![e_ours]);
    let theirs = archive_with(vec![e_theirs]);

    let (merged, conflicts) = merge_entities(&base, &ours, &theirs);
    assert!(conflicts.is_empty(), "no conflicts expected: {conflicts:?}");
    let props = merged[0]
        .properties
        .as_ref()
        .expect("merged has properties");
    assert_eq!(props.get("year").and_then(|v| v.as_str()), Some("2023"));
    assert_eq!(
        props.get("author").and_then(|v| v.as_str()),
        Some("Smith"),
        "theirs-only key 'author' must be preserved in merged output"
    );
}

#[test]
fn independent_property_edits_merge_against_base() {
    let id = Uuid::new_v4();
    let mut common = entity(id, "E");
    common.properties = Some(serde_json::json!({"x": 0, "y": 0}));
    let mut ours_entity = common.clone();
    ours_entity.properties = Some(serde_json::json!({"x": 1, "y": 0}));
    let mut theirs_entity = common.clone();
    theirs_entity.properties = Some(serde_json::json!({"x": 0, "y": 1}));

    let (merged, conflicts) = merge_entities(
        &archive_with(vec![common]),
        &archive_with(vec![ours_entity]),
        &archive_with(vec![theirs_entity]),
    );
    assert!(conflicts.is_empty(), "independent edits: {conflicts:?}");
    assert_eq!(merged.len(), 1);
    assert_eq!(
        merged[0].properties,
        Some(serde_json::json!({"x": 1, "y": 1}))
    );
}

#[test]
fn one_sided_property_removal_survives_other_side_tag_edit() {
    let id = Uuid::new_v4();
    let mut common = entity(id, "E");
    common.properties = Some(serde_json::json!({"k": 1}));
    let mut ours_entity = common.clone();
    ours_entity.properties = Some(serde_json::json!({}));
    let mut theirs_entity = common.clone();
    theirs_entity.tags = vec!["t".into()];

    let (merged, conflicts) = merge_entities(
        &archive_with(vec![common]),
        &archive_with(vec![ours_entity]),
        &archive_with(vec![theirs_entity]),
    );
    assert!(conflicts.is_empty(), "one-sided removal: {conflicts:?}");
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].properties, Some(serde_json::json!({})));
    assert_eq!(merged[0].tags, vec!["t"]);
}
