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
        keys: Vec::new(),
        aliases: Vec::new(),
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

fn alias(
    name: &str,
    origin: khive_storage::entity::EntityAliasOrigin,
) -> khive_storage::entity::EntityAlias {
    khive_storage::entity::EntityAlias {
        alias: name.into(),
        origin,
    }
}

#[test]
fn a_key_or_alias_added_on_one_side_is_not_an_unchanged_entity() {
    use khive_storage::entity::EntityAliasOrigin::Caller;
    let id = Uuid::new_v4();
    let common = entity(id, "E");
    let mut keyed = common.clone();
    keyed.keys = vec!["k".into()];
    let mut aliased = common.clone();
    aliased.aliases = vec![alias("X", Caller)];

    for branch in [keyed, aliased] {
        let (merged, conflicts) = merge_entities(
            &archive_with(vec![common.clone()]),
            &archive_with(vec![branch.clone()]),
            &archive_with(vec![common.clone()]),
        );
        assert!(conflicts.is_empty(), "{conflicts:?}");
        assert_eq!(merged[0].keys, branch.keys);
        assert_eq!(merged[0].aliases, branch.aliases);
    }
}

#[test]
fn keys_merge_as_sets_against_the_base_beside_an_edit_on_the_other_side() {
    let id = Uuid::new_v4();
    let mut common = entity(id, "E");
    common.keys = vec!["kept".into(), "released".into()];
    let mut ours_entity = common.clone();
    ours_entity.keys = vec!["added".into(), "kept".into()];
    let mut theirs_entity = common.clone();
    theirs_entity.tags = vec!["t".into()];

    let (merged, conflicts) = merge_entities(
        &archive_with(vec![common]),
        &archive_with(vec![ours_entity]),
        &archive_with(vec![theirs_entity]),
    );
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_eq!(
        merged[0].keys,
        vec!["added".to_string(), "kept".to_string()]
    );
    assert_eq!(merged[0].tags, vec!["t"]);
}

#[test]
fn aliases_merge_against_the_base_and_a_divergent_origin_resolves_to_ours() {
    use khive_storage::entity::EntityAliasOrigin::{Caller, Merge, Rename};
    let id = Uuid::new_v4();
    let mut common = entity(id, "E");
    common.aliases = vec![alias("dropped", Caller), alias("shared", Caller)];
    let mut ours_entity = common.clone();
    ours_entity.aliases = vec![alias("ours_new", Caller), alias("shared", Rename)];
    let mut theirs_entity = common.clone();
    theirs_entity.aliases = vec![
        alias("dropped", Caller),
        alias("shared", Merge),
        alias("theirs_new", Merge),
    ];

    let (merged, conflicts) = merge_entities(
        &archive_with(vec![common]),
        &archive_with(vec![ours_entity]),
        &archive_with(vec![theirs_entity]),
    );
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_eq!(
        merged[0].aliases,
        vec![
            alias("ours_new", Caller),
            alias("shared", Rename),
            alias("theirs_new", Merge),
        ]
    );
}

#[test]
fn an_alias_equal_to_the_merged_name_is_not_kept() {
    use khive_storage::entity::EntityAliasOrigin::Caller;
    let id = Uuid::new_v4();
    let common = entity(id, "E");
    let mut ours_entity = common.clone();
    ours_entity.tags = vec!["t".into()];
    let mut theirs_entity = common.clone();
    theirs_entity.aliases = vec![alias("E", Caller), alias("F", Caller)];

    let (merged, conflicts) = merge_entities(
        &archive_with(vec![common]),
        &archive_with(vec![ours_entity]),
        &archive_with(vec![theirs_entity]),
    );
    assert!(conflicts.is_empty(), "{conflicts:?}");
    assert_eq!(merged[0].aliases, vec![alias("F", Caller)]);
}

#[test]
fn key_conflicts_report_each_shared_key_once_in_sorted_order() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    let mut ea = entity(a, "A");
    ea.keys = vec!["k2".into(), "k1".into()];
    let mut eb = entity(b, "B");
    eb.keys = vec!["k1".into()];
    let mut ec = entity(c, "C");
    ec.keys = vec!["k1".into(), "k2".into(), "solo".into()];
    ec.kind = "concept".into();

    let conflicts = key_conflicts(&[ec, eb, ea]);
    let mut k1 = vec![a, b, c];
    k1.sort();
    let mut k2 = vec![a, c];
    k2.sort();
    let rendered: Vec<_> = conflicts
        .into_iter()
        .map(|conflict| match conflict {
            MergeConflict::KeyConflict {
                kind,
                key,
                entities,
            } => (kind, key, entities),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(
        rendered,
        vec![
            ("concept".to_string(), "k1".to_string(), k1),
            ("concept".to_string(), "k2".to_string(), k2),
        ]
    );
}
