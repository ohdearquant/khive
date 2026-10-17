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

/// Selects a property value while leaving conflict reporting to the caller.
pub(crate) fn select_property_value<'a>(
    base: Option<&'a serde_json::Value>,
    ours: Option<&'a serde_json::Value>,
    theirs: Option<&'a serde_json::Value>,
    on_conflict: impl FnOnce(),
) -> Option<&'a serde_json::Value> {
    if ours == theirs {
        ours
    } else if ours == base {
        theirs
    } else if theirs == base {
        ours
    } else {
        on_conflict();
        ours
    }
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
        let selected = select_property_value(base_value, ours_value, theirs_value, || {
            conflicts.push(MergeConflict::PropertyMismatch {
                entity_id: id,
                key: key.clone(),
                ours: ours_value.cloned().unwrap_or(Value::Null),
                theirs: theirs_value.cloned().unwrap_or(Value::Null),
            });
        });

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
#[path = "entity_tests.rs"]
mod tests;
