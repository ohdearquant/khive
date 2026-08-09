// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! Edge-level three-way merge and dangling-edge validation.
//!
//! See `crates/khive-merge/docs/api/edge-merge.md` for the decision table.

use std::collections::{HashMap, HashSet};

use khive_runtime::portability::{ExportedEdge, KgArchive};
use uuid::Uuid;

use crate::diff_local::{diff_edges, properties_equal, EdgeChange, EdgeKey};
use crate::types::{BranchSide, MergeConflict, MergeError};

/// Merges edges from `base`, `ours`, and `theirs` by semantic edge key.
///
/// Returns the provisional edge set and any typed edge conflicts. Call
/// [`validate_dangling_edges`] after entity merge before accepting the set.
///
/// # Errors
///
/// The current edge pass is infallible after top-level input validation; the
/// `Result` shape remains part of the merge-layer contract. Use the top-level
/// merge to validate namespaces, weights, and duplicates. See
/// `crates/khive-merge/docs/api/edge-merge.md` for all merge rules.
pub fn merge_edges(
    base: &KgArchive,
    ours: &KgArchive,
    theirs: &KgArchive,
) -> Result<(Vec<ExportedEdge>, Vec<MergeConflict>), MergeError> {
    let ours_diff = diff_edges(base, ours)?;
    let theirs_diff = diff_edges(base, theirs)?;

    let all_keys: HashSet<EdgeKey> = ours_diff
        .keys()
        .chain(theirs_diff.keys())
        .cloned()
        .collect();
    // Sort for deterministic output ordering (AUD-006).
    let mut all_keys_sorted: Vec<EdgeKey> = all_keys.into_iter().collect();
    all_keys_sorted.sort_by(|a, b| {
        a.source
            .cmp(&b.source)
            .then(a.target.cmp(&b.target))
            .then(a.relation.cmp(&b.relation))
    });

    let mut merged: Vec<ExportedEdge> = Vec::new();
    let mut conflicts: Vec<MergeConflict> = Vec::new();

    // Preserve the common-base record for unchanged output.
    let base_edge_map: HashMap<EdgeKey, &ExportedEdge> = base
        .edges
        .iter()
        .map(|e| (EdgeKey::from_edge(e), e))
        .collect();

    for key in &all_keys_sorted {
        let ours_change = ours_diff.get(key);
        let theirs_change = theirs_diff.get(key);

        match (ours_change, theirs_change) {
            (Some(EdgeChange::Unchanged), Some(EdgeChange::Unchanged)) => {
                if let Some(&e) = base_edge_map.get(key) {
                    merged.push(e.clone());
                }
            }

            (Some(EdgeChange::Added(e)), None)
            | (Some(EdgeChange::Added(e)), Some(EdgeChange::Unchanged)) => {
                merged.push(e.clone());
            }

            (None, Some(EdgeChange::Added(e)))
            | (Some(EdgeChange::Unchanged), Some(EdgeChange::Added(e))) => {
                merged.push(e.clone());
            }

            (Some(EdgeChange::Added(e_ours)), Some(EdgeChange::Added(e_theirs))) => {
                let (edge, edge_conflicts) = merge_added_edges(key, e_ours, e_theirs);
                merged.push(edge);
                conflicts.extend(edge_conflicts);
            }

            (Some(EdgeChange::Deleted), Some(EdgeChange::Deleted)) => {}

            (Some(EdgeChange::Deleted), Some(EdgeChange::Unchanged))
            | (Some(EdgeChange::Deleted), None) => {}

            (Some(EdgeChange::Unchanged), Some(EdgeChange::Deleted))
            | (None, Some(EdgeChange::Deleted)) => {}

            (
                Some(EdgeChange::Modified {
                    branch: edge_ours, ..
                }),
                Some(EdgeChange::Unchanged),
            )
            | (
                Some(EdgeChange::Modified {
                    branch: edge_ours, ..
                }),
                None,
            ) => {
                merged.push(edge_ours.clone());
            }

            (
                Some(EdgeChange::Unchanged),
                Some(EdgeChange::Modified {
                    branch: edge_theirs,
                    ..
                }),
            )
            | (
                None,
                Some(EdgeChange::Modified {
                    branch: edge_theirs,
                    ..
                }),
            ) => {
                merged.push(edge_theirs.clone());
            }

            (
                Some(EdgeChange::Modified {
                    base,
                    branch: edge_ours,
                }),
                Some(EdgeChange::Modified {
                    branch: edge_theirs,
                    ..
                }),
            ) => {
                let (edge, edge_conflicts) =
                    merge_modified_edges(key, base, edge_ours, edge_theirs);
                merged.push(edge);
                conflicts.extend(edge_conflicts);
            }

            (Some(EdgeChange::Deleted), Some(EdgeChange::Modified { .. })) => {
                conflicts.push(MergeConflict::EdgeModifyDelete {
                    source_id: key.source,
                    target_id: key.target,
                    relation: key.relation.clone(),
                    modified_in: BranchSide::Theirs,
                    deleted_in: BranchSide::Ours,
                });
            }

            (Some(EdgeChange::Modified { .. }), Some(EdgeChange::Deleted)) => {
                conflicts.push(MergeConflict::EdgeModifyDelete {
                    source_id: key.source,
                    target_id: key.target,
                    relation: key.relation.clone(),
                    modified_in: BranchSide::Ours,
                    deleted_in: BranchSide::Theirs,
                });
            }

            _ => {}
        }
    }

    Ok((merged, conflicts))
}

/// Reconciles two independently added records for one semantic edge key.
///
/// Independent additions have no common durable UUID, so the established
/// policy keeps ours' identity and timestamps and takes the maximum weight.
/// Object properties reconcile per key; same-key divergence remains a typed
/// conflict rather than being silently discarded.
fn merge_added_edges(
    key: &EdgeKey,
    ours: &ExportedEdge,
    theirs: &ExportedEdge,
) -> (ExportedEdge, Vec<MergeConflict>) {
    let mut result = ours.clone();
    result.weight = f64::max(ours.weight, theirs.weight);
    let no_base = None;
    let (properties, conflicts) =
        merge_edge_properties(key, &no_base, &ours.properties, &theirs.properties);
    result.properties = properties;

    (result, conflicts)
}

/// Three-way reconciliation for two modified records of one semantic edge.
///
/// Weight follows the existing maximum policy only when both branches changed
/// it; a one-sided increase or decrease is retained exactly. Property objects
/// reconcile per key, while durable UUIDs use ordinary three-way selection;
/// each reports a typed conflict only when both branches changed the same
/// governed value differently. Ours' complete timestamp pair is the
/// deterministic provenance carrier for a double-modified result; timestamps
/// never create a change or conflict alone.
fn merge_modified_edges(
    key: &EdgeKey,
    base: &ExportedEdge,
    ours: &ExportedEdge,
    theirs: &ExportedEdge,
) -> (ExportedEdge, Vec<MergeConflict>) {
    let mut result = ours.clone();
    let mut conflicts = Vec::new();

    let ours_weight_changed = !weights_equal(base.weight, ours.weight);
    let theirs_weight_changed = !weights_equal(base.weight, theirs.weight);
    result.weight = match (ours_weight_changed, theirs_weight_changed) {
        (false, false) => base.weight,
        (true, false) => ours.weight,
        (false, true) => theirs.weight,
        (true, true) => f64::max(ours.weight, theirs.weight),
    };

    let (properties, property_conflicts) =
        merge_edge_properties(key, &base.properties, &ours.properties, &theirs.properties);
    result.properties = properties;
    conflicts.extend(property_conflicts);

    let ours_identity_changed = base.edge_id != ours.edge_id;
    let theirs_identity_changed = base.edge_id != theirs.edge_id;
    result.edge_id = match (ours_identity_changed, theirs_identity_changed) {
        (false, false) => base.edge_id,
        (true, false) => ours.edge_id,
        (false, true) => theirs.edge_id,
        (true, true) if ours.edge_id == theirs.edge_id => ours.edge_id,
        (true, true) => {
            conflicts.push(MergeConflict::EdgeIdentityMismatch {
                source_id: key.source,
                target_id: key.target,
                relation: key.relation.clone(),
                ours: ours.edge_id,
                theirs: theirs.edge_id,
            });
            ours.edge_id
        }
    };

    (result, conflicts)
}

/// Three-way merges governed edge metadata without discarding independent keys.
///
/// The accepted wire/storage contract makes properties an object. `None` is
/// treated as an empty map for key-level reconciliation; non-object legacy
/// payloads retain conservative atomic selection. A divergent same-key edit
/// produces one payload-level conflict and keeps ours for that key in the
/// provisional record.
fn merge_edge_properties(
    key: &EdgeKey,
    base: &Option<serde_json::Value>,
    ours: &Option<serde_json::Value>,
    theirs: &Option<serde_json::Value>,
) -> (Option<serde_json::Value>, Vec<MergeConflict>) {
    if properties_equal(ours, theirs) {
        return (ours.clone(), Vec::new());
    }
    if properties_equal(ours, base) {
        return (theirs.clone(), Vec::new());
    }
    if properties_equal(theirs, base) {
        return (ours.clone(), Vec::new());
    }

    let all_object_like = [base, ours, theirs]
        .into_iter()
        .all(|value| matches!(value, None | Some(serde_json::Value::Object(_))));
    if !all_object_like {
        return (
            ours.clone(),
            vec![edge_property_conflict(key, ours, theirs)],
        );
    }

    let base_object = base.as_ref().and_then(serde_json::Value::as_object);
    let ours_object = ours.as_ref().and_then(serde_json::Value::as_object);
    let theirs_object = theirs.as_ref().and_then(serde_json::Value::as_object);
    let mut property_keys: HashSet<String> = HashSet::new();
    for object in [base_object, ours_object, theirs_object]
        .into_iter()
        .flatten()
    {
        property_keys.extend(object.keys().cloned());
    }
    let mut property_keys: Vec<String> = property_keys.into_iter().collect();
    property_keys.sort();

    let mut merged = serde_json::Map::new();
    let mut has_conflict = false;
    for property_key in property_keys {
        let base_value = base_object.and_then(|object| object.get(&property_key));
        let ours_value = ours_object.and_then(|object| object.get(&property_key));
        let theirs_value = theirs_object.and_then(|object| object.get(&property_key));
        let selected = if ours_value == theirs_value {
            ours_value
        } else if ours_value == base_value {
            theirs_value
        } else if theirs_value == base_value {
            ours_value
        } else {
            has_conflict = true;
            ours_value
        };

        if let Some(value) = selected {
            merged.insert(property_key, value.clone());
        }
    }

    let merged = if merged.is_empty() && (ours.is_none() || theirs.is_none()) {
        None
    } else {
        Some(serde_json::Value::Object(merged))
    };
    let conflicts = if has_conflict {
        vec![edge_property_conflict(key, ours, theirs)]
    } else {
        Vec::new()
    };
    (merged, conflicts)
}

fn edge_property_conflict(
    key: &EdgeKey,
    ours: &Option<serde_json::Value>,
    theirs: &Option<serde_json::Value>,
) -> MergeConflict {
    MergeConflict::EdgePropertyMismatch {
        source_id: key.source,
        target_id: key.target,
        relation: key.relation.clone(),
        ours: ours.clone(),
        theirs: theirs.clone(),
    }
}

fn weights_equal(a: f64, b: f64) -> bool {
    (a - b).abs() < f64::EPSILON
}

/// Reports edges whose source or target is absent from `entity_ids`.
///
/// Call this after entity merge; when both endpoints are missing, the source
/// is reported first. See `crates/khive-merge/docs/api/edge-merge.md`.
pub fn validate_dangling_edges(
    edges: &[ExportedEdge],
    entity_ids: &HashSet<Uuid>,
) -> Vec<MergeConflict> {
    let mut conflicts = Vec::new();
    for edge in edges {
        if !entity_ids.contains(&edge.source) {
            conflicts.push(MergeConflict::DanglingEdge {
                source_id: edge.source,
                target_id: edge.target,
                relation: edge.relation.to_string(),
                missing_endpoint: edge.source,
            });
        } else if !entity_ids.contains(&edge.target) {
            conflicts.push(MergeConflict::DanglingEdge {
                source_id: edge.source,
                target_id: edge.target,
                relation: edge.relation.to_string(),
                missing_endpoint: edge.target,
            });
        }
    }
    conflicts
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use khive_runtime::portability::{ExportedEdge, KgArchive};
    use khive_storage::EdgeRelation;
    use uuid::Uuid;

    use super::*;

    fn archive(edges: Vec<ExportedEdge>) -> KgArchive {
        KgArchive {
            format: "khive-kg".into(),
            version: "0.1".into(),
            namespace: "test".into(),
            exported_at: Utc::now(),
            entities: vec![],
            edges,
        }
    }

    fn edge(src: Uuid, tgt: Uuid, weight: f64) -> ExportedEdge {
        ExportedEdge {
            edge_id: Uuid::new_v4(),
            source: src,
            target: tgt,
            relation: EdgeRelation::Extends,
            weight,
            properties: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn added_in_ours_included() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let base = archive(vec![]);
        let ours = archive(vec![edge(a, b, 1.0)]);
        let theirs = archive(vec![]);
        let (merged, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();
        assert!(conflicts.is_empty());
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn deleted_in_both_excluded() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let base = archive(vec![edge(a, b, 1.0)]);
        let ours = archive(vec![]);
        let theirs = archive(vec![]);
        let (merged, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();
        assert!(conflicts.is_empty());
        assert_eq!(merged.len(), 0);
    }

    #[test]
    fn max_weight_on_both_added() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let base = archive(vec![]);
        let ours = archive(vec![edge(a, b, 0.6)]);
        let theirs = archive(vec![edge(a, b, 0.9)]);
        let (merged, _) = merge_edges(&base, &ours, &theirs).unwrap();
        assert_eq!(merged.len(), 1);
        assert!((merged[0].weight - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn dangling_edge_detected() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let edges = vec![edge(a, b, 1.0)];
        let entity_ids: HashSet<Uuid> = [a].into_iter().collect();
        let conflicts = validate_dangling_edges(&edges, &entity_ids);
        assert_eq!(conflicts.len(), 1);
        assert!(
            matches!(conflicts[0], MergeConflict::DanglingEdge { missing_endpoint, .. } if missing_endpoint == b)
        );
    }

    #[test]
    fn edge_modify_delete_conflict() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let base = archive(vec![edge(a, b, 0.5)]);
        let ours = archive(vec![]);
        let theirs = archive(vec![edge(a, b, 1.0)]);

        let (_, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert!(matches!(
            conflicts[0],
            MergeConflict::EdgeModifyDelete { .. }
        ));
    }

    #[test]
    fn merge_preserves_added_edge_id() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let branch_edge = edge(a, b, 1.0);
        let expected_id = branch_edge.edge_id;

        let base = archive(vec![]);
        let ours = archive(vec![branch_edge]);
        let theirs = archive(vec![]);

        let (merged, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();
        assert!(conflicts.is_empty());
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].edge_id, expected_id,
            "merged edge_id must equal the branch's edge_id, not a fresh UUID"
        );
    }

    #[test]
    fn merge_preserves_weight_modified_edge_id() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        let base_edge = ExportedEdge {
            edge_id: Uuid::new_v4(),
            source: a,
            target: b,
            relation: EdgeRelation::Extends,
            weight: 0.5,
            properties: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let ours_edge = ExportedEdge {
            edge_id: Uuid::new_v4(),
            source: a,
            target: b,
            relation: EdgeRelation::Extends,
            weight: 0.9,
            properties: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let expected_id = ours_edge.edge_id;

        let base = archive(vec![base_edge.clone()]);
        let ours = archive(vec![ours_edge]);
        let theirs = archive(vec![base_edge]);

        let (merged, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();
        assert!(conflicts.is_empty());
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].weight, 0.9);
        assert_eq!(
            merged[0].edge_id, expected_id,
            "merged edge_id must equal ours' edge_id after weight modification"
        );
    }

    #[test]
    fn one_sided_property_change_preserves_complete_branch_edge() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let base_edge = edge(a, b, 0.7);
        let mut ours_edge = base_edge.clone();
        ours_edge.properties = Some(serde_json::json!({"confidence": 0.95}));
        ours_edge.created_at = chrono::DateTime::parse_from_rfc3339("2026-03-03T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        ours_edge.updated_at = chrono::DateTime::parse_from_rfc3339("2026-04-04T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let base = archive(vec![base_edge.clone()]);
        let ours = archive(vec![ours_edge.clone()]);
        let theirs = archive(vec![base_edge]);
        let (merged, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();

        assert!(conflicts.is_empty(), "unexpected conflicts: {conflicts:?}");
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].edge_id, ours_edge.edge_id);
        assert_eq!(merged[0].properties, ours_edge.properties);
        assert_eq!(merged[0].created_at, ours_edge.created_at);
        assert_eq!(merged[0].updated_at, ours_edge.updated_at);
    }

    #[test]
    fn divergent_property_only_changes_report_typed_conflict() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut base_edge = edge(a, b, 0.7);
        base_edge.properties = Some(serde_json::json!({"confidence": 0.5}));
        let mut ours_edge = base_edge.clone();
        ours_edge.properties = Some(serde_json::json!({"confidence": 0.8}));
        let mut theirs_edge = base_edge.clone();
        theirs_edge.properties = Some(serde_json::json!({"confidence": 0.9}));

        let base = archive(vec![base_edge]);
        let ours = archive(vec![ours_edge]);
        let theirs = archive(vec![theirs_edge]);
        let (_, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();

        assert!(matches!(
            conflicts.as_slice(),
            [MergeConflict::EdgePropertyMismatch {
                source_id,
                target_id,
                relation,
                ours: Some(ours),
                theirs: Some(theirs),
            }] if *source_id == a
                && *target_id == b
                && relation == "extends"
                && ours == &serde_json::json!({"confidence": 0.8})
                && theirs == &serde_json::json!({"confidence": 0.9})
        ));
    }

    #[test]
    fn independent_property_key_changes_merge_without_loss() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut base_edge = edge(a, b, 0.7);
        base_edge.properties = Some(serde_json::json!({"shared": "base"}));
        let mut ours_edge = base_edge.clone();
        ours_edge.properties = Some(serde_json::json!({
            "ours": 1,
            "shared": "base",
        }));
        let mut theirs_edge = base_edge.clone();
        theirs_edge.properties = Some(serde_json::json!({
            "shared": "base",
            "theirs": 2,
        }));

        let base = archive(vec![base_edge]);
        let ours = archive(vec![ours_edge]);
        let theirs = archive(vec![theirs_edge]);
        let (merged, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();

        assert!(conflicts.is_empty(), "unexpected conflicts: {conflicts:?}");
        assert_eq!(
            merged[0].properties,
            Some(serde_json::json!({
                "ours": 1,
                "shared": "base",
                "theirs": 2,
            }))
        );
    }

    #[test]
    fn independent_weight_and_property_changes_merge_without_loss() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut base_edge = edge(a, b, 0.5);
        base_edge.properties = Some(serde_json::json!({"origin": "base"}));
        let mut ours_edge = base_edge.clone();
        ours_edge.properties = Some(serde_json::json!({"origin": "ours"}));
        let mut theirs_edge = base_edge.clone();
        theirs_edge.weight = 0.2;

        let base = archive(vec![base_edge]);
        let ours = archive(vec![ours_edge.clone()]);
        let theirs = archive(vec![theirs_edge]);
        let (merged, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();

        assert!(conflicts.is_empty(), "unexpected conflicts: {conflicts:?}");
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].weight, 0.2);
        assert_eq!(merged[0].properties, ours_edge.properties);
        assert_eq!(merged[0].created_at, ours_edge.created_at);
        assert_eq!(merged[0].updated_at, ours_edge.updated_at);
    }

    #[test]
    fn divergent_edge_identity_changes_report_typed_conflict() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let base_edge = edge(a, b, 0.7);
        let mut ours_edge = base_edge.clone();
        ours_edge.edge_id = Uuid::new_v4();
        let mut theirs_edge = base_edge.clone();
        theirs_edge.edge_id = Uuid::new_v4();

        let base = archive(vec![base_edge]);
        let ours = archive(vec![ours_edge.clone()]);
        let theirs = archive(vec![theirs_edge.clone()]);
        let (_, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();

        assert!(matches!(
            conflicts.as_slice(),
            [MergeConflict::EdgeIdentityMismatch {
                source_id,
                target_id,
                relation,
                ours,
                theirs,
            }] if *source_id == a
                && *target_id == b
                && relation == "extends"
                && *ours == ours_edge.edge_id
                && *theirs == theirs_edge.edge_id
        ));
    }

    #[test]
    fn property_modify_delete_is_an_edge_modify_delete_conflict() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let base_edge = edge(a, b, 0.7);
        let mut theirs_edge = base_edge.clone();
        theirs_edge.properties = Some(serde_json::json!({"confidence": 0.95}));

        let base = archive(vec![base_edge]);
        let ours = archive(vec![]);
        let theirs = archive(vec![theirs_edge]);
        let (_, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();

        assert!(matches!(
            conflicts.as_slice(),
            [MergeConflict::EdgeModifyDelete {
                modified_in: BranchSide::Theirs,
                deleted_in: BranchSide::Ours,
                ..
            }]
        ));
    }
}
