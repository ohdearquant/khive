// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! Entity-level three-way merge and field-level conflict analysis.
//!
//! See `crates/khive-merge/docs/api/entity-merge.md` for the decision table.

use std::collections::{HashMap, HashSet};

use khive_runtime::portability::{ExportedEntity, KgArchive};
use uuid::Uuid;

use crate::diff_local::{diff_entities, properties_equal, EntityChange};
use crate::types::{BranchSide, MergeConflict};

/// Merges all entity UUIDs and returns a provisional set plus typed conflicts.
///
/// Conflicted double modifications retain ours as a provisional fallback.
/// See `crates/khive-merge/docs/api/entity-merge.md` for field rules.
pub fn merge_entities(
    base: &KgArchive,
    ours: &KgArchive,
    theirs: &KgArchive,
) -> (Vec<ExportedEntity>, Vec<MergeConflict>) {
    let ours_diff = diff_entities(base, ours);
    let theirs_diff = diff_entities(base, theirs);

    let all_ids: HashSet<Uuid> = ours_diff
        .keys()
        .chain(theirs_diff.keys())
        .copied()
        .collect();
    // Sort for deterministic output ordering (AUD-006).
    let mut all_ids_sorted: Vec<Uuid> = all_ids.into_iter().collect();
    all_ids_sorted.sort();

    let mut merged: Vec<ExportedEntity> = Vec::new();
    let mut conflicts: Vec<MergeConflict> = Vec::new();

    let base_map: HashMap<Uuid, &ExportedEntity> =
        base.entities.iter().map(|e| (e.id, e)).collect();

    for id in &all_ids_sorted {
        let ours_change = ours_diff.get(id);
        let theirs_change = theirs_diff.get(id);

        match (ours_change, theirs_change) {
            (Some(EntityChange::Unchanged), Some(EntityChange::Unchanged)) => {
                if let Some(&e) = base_map.get(id) {
                    merged.push(e.clone());
                }
            }

            (Some(EntityChange::Added(e)), None)
            | (Some(EntityChange::Added(e)), Some(EntityChange::Unchanged)) => {
                merged.push(e.clone());
            }

            (None, Some(EntityChange::Added(e)))
            | (Some(EntityChange::Unchanged), Some(EntityChange::Added(e))) => {
                merged.push(e.clone());
            }

            (Some(EntityChange::Added(e_ours)), Some(EntityChange::Added(e_theirs))) => {
                let diffs = detect_entity_diffs(e_ours, e_theirs);
                if diffs.is_empty() {
                    merged.push(e_ours.clone());
                } else {
                    conflicts.push(MergeConflict::DuplicateAddition {
                        entity_id: *id,
                        differing_fields: diffs,
                    });
                    merged.push(e_ours.clone());
                }
            }

            (Some(EntityChange::Deleted), Some(EntityChange::Deleted)) => {}

            (Some(EntityChange::Deleted), Some(EntityChange::Unchanged))
            | (Some(EntityChange::Deleted), None) => {}

            (Some(EntityChange::Unchanged), Some(EntityChange::Deleted))
            | (None, Some(EntityChange::Deleted)) => {}

            (
                Some(EntityChange::Modified { branch: e_ours, .. }),
                Some(EntityChange::Unchanged),
            )
            | (Some(EntityChange::Modified { branch: e_ours, .. }), None) => {
                merged.push(e_ours.clone());
            }

            (
                Some(EntityChange::Unchanged),
                Some(EntityChange::Modified {
                    branch: e_theirs, ..
                }),
            )
            | (
                None,
                Some(EntityChange::Modified {
                    branch: e_theirs, ..
                }),
            ) => {
                merged.push(e_theirs.clone());
            }

            (
                Some(EntityChange::Modified {
                    base: e_base,
                    branch: e_ours,
                }),
                Some(EntityChange::Modified {
                    base: _,
                    branch: e_theirs,
                }),
            ) => {
                let (entity_result, field_conflicts) =
                    field_level_merge(*id, e_base, e_ours, e_theirs);
                if field_conflicts.is_empty() {
                    merged.push(entity_result);
                } else {
                    conflicts.extend(field_conflicts);
                    // Preserve a deterministic provisional value for resolution UX.
                    merged.push(e_ours.clone());
                }
            }

            (Some(EntityChange::Deleted), Some(EntityChange::Modified { .. })) => {
                conflicts.push(MergeConflict::ModifyDelete {
                    entity_id: *id,
                    modified_in: BranchSide::Theirs,
                    deleted_in: BranchSide::Ours,
                });
            }

            (Some(EntityChange::Modified { .. }), Some(EntityChange::Deleted)) => {
                conflicts.push(MergeConflict::ModifyDelete {
                    entity_id: *id,
                    modified_in: BranchSide::Ours,
                    deleted_in: BranchSide::Theirs,
                });
            }

            _ => {}
        }
    }

    (merged, conflicts)
}

/// Reconciles one double-modified entity and reports unresolvable fields.
fn field_level_merge(
    id: Uuid,
    base: &ExportedEntity,
    ours: &ExportedEntity,
    theirs: &ExportedEntity,
) -> (ExportedEntity, Vec<MergeConflict>) {
    let mut conflicts = Vec::new();
    let mut result = ours.clone();

    if ours.name != theirs.name {
        conflicts.push(MergeConflict::NameConflict {
            entity_id: id,
            ours: ours.name.clone(),
            theirs: theirs.name.clone(),
        });
    }

    if ours.kind != theirs.kind {
        conflicts.push(MergeConflict::KindConflict {
            entity_id: id,
            ours: ours.kind.clone(),
            theirs: theirs.kind.clone(),
        });
    }

    // Reuse the property conflict payload for the governed subtype.
    if ours.entity_type != theirs.entity_type {
        conflicts.push(MergeConflict::PropertyMismatch {
            entity_id: id,
            key: "entity_type".into(),
            ours: serde_json::json!(&ours.entity_type),
            theirs: serde_json::json!(&theirs.entity_type),
        });
    }

    if ours.description != theirs.description {
        // Description is annotation rather than identity, so ours wins.
        result.description = ours.description.clone();
    }

    {
        let mut tag_set: HashSet<String> = ours.tags.iter().cloned().collect();
        for t in &theirs.tags {
            tag_set.insert(t.clone());
        }
        let mut tags: Vec<String> = tag_set.into_iter().collect();
        tags.sort();
        result.tags = tags;
    }

    let (merged_props, prop_conflicts) =
        merge_properties(id, &base.properties, &ours.properties, &theirs.properties);
    result.properties = merged_props;
    conflicts.extend(prop_conflicts);

    (result, conflicts)
}

/// Merges object properties per key against the common base.
///
/// Absent keys are values in the three-way comparison, so an unchanged branch
/// cannot restore a key deleted on the other branch. Legacy non-object payloads
/// are selected atomically when one-sided and conflict when both sides diverge.
fn merge_properties(
    id: Uuid,
    base_props: &Option<serde_json::Value>,
    ours_props: &Option<serde_json::Value>,
    theirs_props: &Option<serde_json::Value>,
) -> (Option<serde_json::Value>, Vec<MergeConflict>) {
    use serde_json::{Map, Value};

    if properties_equal(ours_props, theirs_props) {
        return (ours_props.clone(), vec![]);
    }
    if properties_equal(ours_props, base_props) {
        return (theirs_props.clone(), vec![]);
    }
    if properties_equal(theirs_props, base_props) {
        return (ours_props.clone(), vec![]);
    }

    let all_object_like = [base_props, ours_props, theirs_props]
        .into_iter()
        .all(|props| matches!(props, None | Some(Value::Object(_))));
    if !all_object_like {
        return (
            ours_props.clone(),
            vec![MergeConflict::PropertyMismatch {
                entity_id: id,
                key: "properties".into(),
                ours: ours_props.clone().unwrap_or(Value::Null),
                theirs: theirs_props.clone().unwrap_or(Value::Null),
            }],
        );
    }

    let base_obj = base_props.as_ref().and_then(Value::as_object);
    let ours_obj = ours_props.as_ref().and_then(Value::as_object);
    let theirs_obj = theirs_props.as_ref().and_then(Value::as_object);
    let all_keys: HashSet<&String> = [base_obj, ours_obj, theirs_obj]
        .into_iter()
        .flatten()
        .flat_map(|object| object.keys())
        .collect();
    let mut all_keys_sorted: Vec<&String> = all_keys.into_iter().collect();
    all_keys_sorted.sort();

    let mut merged = Map::new();
    let mut conflicts = Vec::new();
    for key in all_keys_sorted {
        let base_value = base_obj.and_then(|object| object.get(key));
        let ours_value = ours_obj.and_then(|object| object.get(key));
        let theirs_value = theirs_obj.and_then(|object| object.get(key));
        let selected = if ours_value == theirs_value {
            ours_value
        } else if ours_value == base_value {
            theirs_value
        } else if theirs_value == base_value {
            ours_value
        } else {
            conflicts.push(MergeConflict::PropertyMismatch {
                entity_id: id,
                key: key.clone(),
                ours: ours_value.cloned().unwrap_or(Value::Null),
                theirs: theirs_value.cloned().unwrap_or(Value::Null),
            });
            ours_value
        };

        if let Some(value) = selected {
            merged.insert(key.clone(), value.clone());
        }
    }

    let merged = if merged.is_empty() && (ours_props.is_none() || theirs_props.is_none()) {
        None
    } else {
        Some(Value::Object(merged))
    };
    (merged, conflicts)
}

/// Lists content fields that differ for a duplicate UUID addition.
fn detect_entity_diffs(ours: &ExportedEntity, theirs: &ExportedEntity) -> Vec<String> {
    let mut diffs = Vec::new();
    if ours.name != theirs.name {
        diffs.push("name".into());
    }
    if ours.kind != theirs.kind {
        diffs.push("kind".into());
    }
    if ours.entity_type != theirs.entity_type {
        diffs.push("entity_type".into());
    }
    if ours.description != theirs.description {
        diffs.push("description".into());
    }
    if !properties_equal(&ours.properties, &theirs.properties) {
        diffs.push("properties".into());
    }
    let mut ours_tags = ours.tags.clone();
    let mut theirs_tags = theirs.tags.clone();
    ours_tags.sort();
    theirs_tags.sort();
    if ours_tags != theirs_tags {
        diffs.push("tags".into());
    }
    diffs
}

#[cfg(test)]
mod tests {
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
}
