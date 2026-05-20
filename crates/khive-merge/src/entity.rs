// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! Entity-level three-way merge and field-level conflict analysis (ADR-043 §4).

use std::collections::{HashMap, HashSet};

use khive_runtime::portability::{ExportedEntity, KgArchive};
use uuid::Uuid;

use khive_vcs::merge_engine::{BranchSide, MergeConflict};

use crate::diff_local::{diff_entities, EntityChange};

/// Categorize all entity UUIDs across base, ours, theirs and produce:
/// - A set of entities to include in the merged archive (no conflict).
/// - A list of `MergeConflict` values to report.
pub fn merge_entities(
    base: &KgArchive,
    ours: &KgArchive,
    theirs: &KgArchive,
) -> (Vec<ExportedEntity>, Vec<MergeConflict>) {
    let ours_diff = diff_entities(base, ours);
    let theirs_diff = diff_entities(base, theirs);

    let all_ids: HashSet<Uuid> = ours_diff.keys().chain(theirs_diff.keys()).copied().collect();

    let mut merged: Vec<ExportedEntity> = Vec::new();
    let mut conflicts: Vec<MergeConflict> = Vec::new();

    let base_map: HashMap<Uuid, &ExportedEntity> =
        base.entities.iter().map(|e| (e.id, e)).collect();

    for id in &all_ids {
        let ours_change = ours_diff.get(id);
        let theirs_change = theirs_diff.get(id);

        match (ours_change, theirs_change) {
            // Both branches unchanged → include as-is from base.
            (
                Some(EntityChange::Unchanged),
                Some(EntityChange::Unchanged),
            ) => {
                if let Some(&e) = base_map.get(id) {
                    merged.push(e.clone());
                }
            }

            // Added in ours only → include.
            (Some(EntityChange::Added(e)), None)
            | (Some(EntityChange::Added(e)), Some(EntityChange::Unchanged)) => {
                merged.push(e.clone());
            }

            // Added in theirs only → include.
            (None, Some(EntityChange::Added(e)))
            | (Some(EntityChange::Unchanged), Some(EntityChange::Added(e))) => {
                merged.push(e.clone());
            }

            // Added in both (duplicate UUID) → auto-resolve field-by-field.
            (Some(EntityChange::Added(e_ours)), Some(EntityChange::Added(e_theirs))) => {
                merged.push(merge_entity_fields(e_ours, e_theirs));
            }

            // Deleted in both → do not include (no conflict).
            (Some(EntityChange::Deleted), Some(EntityChange::Deleted)) => {}

            // Deleted in ours, unchanged in theirs → delete in merge.
            (Some(EntityChange::Deleted), Some(EntityChange::Unchanged))
            | (Some(EntityChange::Deleted), None) => {}

            // Deleted in theirs, unchanged in ours → delete in merge.
            (Some(EntityChange::Unchanged), Some(EntityChange::Deleted))
            | (None, Some(EntityChange::Deleted)) => {}

            // Modified in ours, unchanged in theirs → take ours.
            (Some(EntityChange::Modified { branch: e_ours, .. }), Some(EntityChange::Unchanged))
            | (Some(EntityChange::Modified { branch: e_ours, .. }), None) => {
                merged.push(e_ours.clone());
            }

            // Modified in theirs, unchanged in ours → take theirs.
            (Some(EntityChange::Unchanged), Some(EntityChange::Modified { branch: e_theirs, .. }))
            | (None, Some(EntityChange::Modified { branch: e_theirs, .. })) => {
                merged.push(e_theirs.clone());
            }

            // Modified in both → field-level conflict analysis.
            (
                Some(EntityChange::Modified {
                    base: _,
                    branch: e_ours,
                }),
                Some(EntityChange::Modified {
                    base: _,
                    branch: e_theirs,
                }),
            ) => {
                let (entity_result, field_conflicts) =
                    field_level_merge(*id, e_ours, e_theirs);
                if field_conflicts.is_empty() {
                    merged.push(entity_result);
                } else {
                    conflicts.extend(field_conflicts);
                    // Include the ours version as a fallback (agent must resolve).
                    merged.push(e_ours.clone());
                }
            }

            // Deleted in ours, modified in theirs → conflict.
            (Some(EntityChange::Deleted), Some(EntityChange::Modified { .. })) => {
                conflicts.push(MergeConflict::ModifyDelete {
                    entity_id: *id,
                    modified_in: BranchSide::Theirs,
                    deleted_in: BranchSide::Ours,
                });
            }

            // Modified in ours, deleted in theirs → conflict.
            (Some(EntityChange::Modified { .. }), Some(EntityChange::Deleted)) => {
                conflicts.push(MergeConflict::ModifyDelete {
                    entity_id: *id,
                    modified_in: BranchSide::Ours,
                    deleted_in: BranchSide::Theirs,
                });
            }

            // All other combos (e.g. both None — impossible given the union of IDs).
            _ => {}
        }
    }

    (merged, conflicts)
}

/// Perform field-level merge for an entity modified in both branches.
///
/// Returns the merged entity and any unresolvable conflicts.
fn field_level_merge(
    id: Uuid,
    ours: &ExportedEntity,
    theirs: &ExportedEntity,
) -> (ExportedEntity, Vec<MergeConflict>) {
    let mut conflicts = Vec::new();
    let mut result = ours.clone(); // Start from ours as the base.

    // Name: conflict if different.
    if ours.name != theirs.name {
        conflicts.push(MergeConflict::NameConflict {
            entity_id: id,
            ours: ours.name.clone(),
            theirs: theirs.name.clone(),
        });
    }

    // Kind: conflict if different.
    if ours.kind != theirs.kind {
        conflicts.push(MergeConflict::KindConflict {
            entity_id: id,
            ours: ours.kind.clone(),
            theirs: theirs.kind.clone(),
        });
    }

    // Description: ours wins (annotation, not identity).
    if ours.description != theirs.description {
        // Auto-resolved: keep ours (no conflict).
        result.description = ours.description.clone();
    }

    // Tags: union.
    {
        let mut tag_set: HashSet<String> = ours.tags.iter().cloned().collect();
        for t in &theirs.tags {
            tag_set.insert(t.clone());
        }
        let mut tags: Vec<String> = tag_set.into_iter().collect();
        tags.sort();
        result.tags = tags;
    }

    // Properties: per-key merge.
    if let Some(prop_conflicts) = merge_properties(id, &ours.properties, &theirs.properties) {
        conflicts.extend(prop_conflicts);
    }
    // result.properties already has ours value; conflicts carry both values for the agent.

    (result, conflicts)
}

/// Merge entity properties from ours and theirs.
/// Returns `None` if no conflicts, or `Some(conflicts)` if there are conflicts.
fn merge_properties(
    id: Uuid,
    ours_props: &Option<serde_json::Value>,
    theirs_props: &Option<serde_json::Value>,
) -> Option<Vec<MergeConflict>> {
    use serde_json::Value;

    let ours_obj = match ours_props {
        Some(Value::Object(m)) => Some(m),
        _ => None,
    };
    let theirs_obj = match theirs_props {
        Some(Value::Object(m)) => Some(m),
        _ => None,
    };

    match (ours_obj, theirs_obj) {
        (None, None) => None,
        (Some(_), None) | (None, Some(_)) => None, // One side has no props → no conflict.
        (Some(o), Some(t)) => {
            let mut conflicts = Vec::new();
            let all_keys: HashSet<&String> = o.keys().chain(t.keys()).collect();

            for key in all_keys {
                match (o.get(key), t.get(key)) {
                    (Some(ov), Some(tv)) if ov != tv => {
                        conflicts.push(MergeConflict::PropertyMismatch {
                            entity_id: id,
                            key: key.clone(),
                            ours: ov.clone(),
                            theirs: tv.clone(),
                        });
                    }
                    _ => {} // Absent in one side, or equal → no conflict.
                }
            }

            if conflicts.is_empty() {
                None
            } else {
                Some(conflicts)
            }
        }
    }
}

/// Auto-merge an entity where both branches added the same UUID (ADR-043 §4.1).
/// Scalars → ours wins; tags → union.
fn merge_entity_fields(ours: &ExportedEntity, theirs: &ExportedEntity) -> ExportedEntity {
    let mut result = ours.clone();
    // Tags: union.
    let mut tag_set: HashSet<String> = ours.tags.iter().cloned().collect();
    for t in &theirs.tags {
        tag_set.insert(t.clone());
    }
    let mut tags: Vec<String> = tag_set.into_iter().collect();
    tags.sort();
    result.tags = tags;
    result
}

// ── Tests ─────────────────────────────────────────────────────────────────────

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
        let theirs = archive_with(vec![]); // deleted theirs

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
        assert!(matches!(conflicts[0], MergeConflict::PropertyMismatch { .. }));
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
}
