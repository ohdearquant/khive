// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! Edge-level three-way merge and dangling-edge validation.
//!
//! See `crates/khive-merge/docs/api/edge-merge.md` for the decision table.

use std::collections::{HashMap, HashSet};

use khive_runtime::portability::{ExportedEdge, KgArchive};
use uuid::Uuid;

use crate::diff_local::{diff_edges, EdgeChange, EdgeKey};
use crate::types::{BranchSide, MergeConflict, MergeError};

/// Merges edges from `base`, `ours`, and `theirs` by semantic edge key.
///
/// Returns the provisional edge set and any modify/delete conflicts. Call
/// [`validate_dangling_edges`] after entity merge before accepting the set.
///
/// # Errors
///
/// Returns [`MergeError::Internal`] if a relation cannot be reconstructed.
/// Use the top-level merge to validate namespaces, weights, and duplicates.
/// See `crates/khive-merge/docs/api/edge-merge.md` for all merge rules.
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

    // Preserve originating edge IDs across merge/diff cycles.
    let base_edge_map: HashMap<EdgeKey, &ExportedEdge> = base
        .edges
        .iter()
        .map(|e| (EdgeKey::from_edge(e), e))
        .collect();
    let ours_edge_map: HashMap<EdgeKey, &ExportedEdge> = ours
        .edges
        .iter()
        .map(|e| (EdgeKey::from_edge(e), e))
        .collect();
    let theirs_edge_map: HashMap<EdgeKey, &ExportedEdge> = theirs
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
                // Simultaneous weight changes auto-resolve to the maximum.
                let weight = f64::max(e_ours.weight, e_theirs.weight);
                let mut edge = e_ours.clone();
                edge.weight = weight;
                merged.push(edge);
            }

            (Some(EdgeChange::Deleted), Some(EdgeChange::Deleted)) => {}

            (Some(EdgeChange::Deleted), Some(EdgeChange::Unchanged))
            | (Some(EdgeChange::Deleted), None) => {}

            (Some(EdgeChange::Unchanged), Some(EdgeChange::Deleted))
            | (None, Some(EdgeChange::Deleted)) => {}

            (
                Some(EdgeChange::WeightModified { branch_weight, .. }),
                Some(EdgeChange::Unchanged),
            )
            | (Some(EdgeChange::WeightModified { branch_weight, .. }), None) => {
                let id = ours_edge_map.get(key).map(|e| e.edge_id);
                let edge = build_edge(key, *branch_weight, id)?;
                merged.push(edge);
            }

            (
                Some(EdgeChange::Unchanged),
                Some(EdgeChange::WeightModified { branch_weight, .. }),
            )
            | (None, Some(EdgeChange::WeightModified { branch_weight, .. })) => {
                let id = theirs_edge_map.get(key).map(|e| e.edge_id);
                let edge = build_edge(key, *branch_weight, id)?;
                merged.push(edge);
            }

            // Prefer ours' ID when both weights changed for deterministic identity.
            (
                Some(EdgeChange::WeightModified {
                    branch_weight: ours_w,
                    ..
                }),
                Some(EdgeChange::WeightModified {
                    branch_weight: theirs_w,
                    ..
                }),
            ) => {
                let id = ours_edge_map
                    .get(key)
                    .or_else(|| theirs_edge_map.get(key))
                    .map(|e| e.edge_id);
                let edge = build_edge(key, f64::max(*ours_w, *theirs_w), id)?;
                merged.push(edge);
            }

            (Some(EdgeChange::Deleted), Some(EdgeChange::WeightModified { .. })) => {
                conflicts.push(MergeConflict::EdgeModifyDelete {
                    source_id: key.source,
                    target_id: key.target,
                    relation: key.relation.clone(),
                    modified_in: BranchSide::Theirs,
                    deleted_in: BranchSide::Ours,
                });
            }

            (Some(EdgeChange::WeightModified { .. }), Some(EdgeChange::Deleted)) => {
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

/// Reconstructs an edge, preserving `existing_id` or minting a fallback UUID.
fn build_edge(
    key: &EdgeKey,
    weight: f64,
    existing_id: Option<Uuid>,
) -> Result<ExportedEdge, MergeError> {
    let relation = key
        .relation
        .parse::<khive_storage::EdgeRelation>()
        .map_err(|e| MergeError::Internal(e.to_string()))?;
    Ok(ExportedEdge {
        edge_id: existing_id.unwrap_or_else(Uuid::new_v4),
        source: key.source,
        target: key.target,
        relation,
        weight,
    })
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
        };
        let ours_edge = ExportedEdge {
            edge_id: Uuid::new_v4(),
            source: a,
            target: b,
            relation: EdgeRelation::Extends,
            weight: 0.9,
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
}
