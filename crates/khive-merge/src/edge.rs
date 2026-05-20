// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! Edge-level three-way merge and dangling-edge validation (ADR-043 §5).

use std::collections::{HashMap, HashSet};

use khive_runtime::portability::{ExportedEdge, KgArchive};
use uuid::Uuid;

use khive_vcs::merge_engine::{BranchSide, MergeConflict};

use crate::diff_local::{diff_edges, EdgeChange, EdgeKey};

/// Merge edges from base, ours, and theirs.
///
/// Returns:
/// - `merged_edges`: edges to include in the merged archive.
/// - `edge_conflicts`: edge-level conflicts.
///
/// Call `validate_dangling_edges` on the merged edge set after entity merge
/// to detect dangling references.
pub fn merge_edges(
    base: &KgArchive,
    ours: &KgArchive,
    theirs: &KgArchive,
) -> (Vec<ExportedEdge>, Vec<MergeConflict>) {
    let ours_diff = diff_edges(base, ours);
    let theirs_diff = diff_edges(base, theirs);

    let all_keys: HashSet<EdgeKey> = ours_diff
        .keys()
        .chain(theirs_diff.keys())
        .cloned()
        .collect();

    let mut merged: Vec<ExportedEdge> = Vec::new();
    let mut conflicts: Vec<MergeConflict> = Vec::new();

    // Build edge lookup from base for unchanged reference.
    let base_edge_map: HashMap<EdgeKey, &ExportedEdge> = base
        .edges
        .iter()
        .map(|e| (EdgeKey::from_edge(e), e))
        .collect();

    for key in &all_keys {
        let ours_change = ours_diff.get(key);
        let theirs_change = theirs_diff.get(key);

        match (ours_change, theirs_change) {
            // Both unchanged → include.
            (Some(EdgeChange::Unchanged), Some(EdgeChange::Unchanged)) => {
                if let Some(&e) = base_edge_map.get(key) {
                    merged.push(e.clone());
                }
            }

            // Added in ours only → include.
            (Some(EdgeChange::Added(e)), None)
            | (Some(EdgeChange::Added(e)), Some(EdgeChange::Unchanged)) => {
                merged.push(e.clone());
            }

            // Added in theirs only → include.
            (None, Some(EdgeChange::Added(e)))
            | (Some(EdgeChange::Unchanged), Some(EdgeChange::Added(e))) => {
                merged.push(e.clone());
            }

            // Added in both with same weight → include once.
            (Some(EdgeChange::Added(e_ours)), Some(EdgeChange::Added(e_theirs))) => {
                // Auto-resolve: max weight wins (ADR-017 §6.2 `duplicate_edge_weight`).
                let weight = f64::max(e_ours.weight, e_theirs.weight);
                let mut edge = e_ours.clone();
                edge.weight = weight;
                merged.push(edge);
            }

            // Deleted in both → exclude.
            (Some(EdgeChange::Deleted), Some(EdgeChange::Deleted)) => {}

            // Deleted in ours, unchanged in theirs → exclude.
            (Some(EdgeChange::Deleted), Some(EdgeChange::Unchanged))
            | (Some(EdgeChange::Deleted), None) => {}

            // Deleted in theirs, unchanged in ours → exclude.
            (Some(EdgeChange::Unchanged), Some(EdgeChange::Deleted))
            | (None, Some(EdgeChange::Deleted)) => {}

            // Weight modified in ours only → take ours.
            (
                Some(EdgeChange::WeightModified { branch_weight, .. }),
                Some(EdgeChange::Unchanged),
            )
            | (Some(EdgeChange::WeightModified { branch_weight, .. }), None) => {
                let edge = build_edge(key, *branch_weight);
                merged.push(edge);
            }

            // Weight modified in theirs only → take theirs.
            (
                Some(EdgeChange::Unchanged),
                Some(EdgeChange::WeightModified { branch_weight, .. }),
            )
            | (None, Some(EdgeChange::WeightModified { branch_weight, .. })) => {
                let edge = build_edge(key, *branch_weight);
                merged.push(edge);
            }

            // Weight modified in both → auto-resolve: max weight.
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
                let edge = build_edge(key, f64::max(*ours_w, *theirs_w));
                merged.push(edge);
            }

            // Deleted in ours, modified in theirs → conflict.
            (Some(EdgeChange::Deleted), Some(EdgeChange::WeightModified { .. })) => {
                conflicts.push(MergeConflict::EdgeModifyDelete {
                    source_id: key.source,
                    target_id: key.target,
                    relation: key.relation.clone(),
                    modified_in: BranchSide::Theirs,
                    deleted_in: BranchSide::Ours,
                });
            }

            // Modified in ours, deleted in theirs → conflict.
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

    (merged, conflicts)
}

/// Validate that no edge in `edges` has a missing endpoint in `entity_ids`.
///
/// Returns dangling-edge conflicts for any edge whose source or target is not
/// in `entity_ids`. Call this after `merge_entities` to get the final entity set.
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

fn build_edge(key: &EdgeKey, weight: f64) -> ExportedEdge {
    ExportedEdge {
        source: key.source,
        target: key.target,
        relation: key
            .relation
            .parse()
            .expect("valid relation from existing edge"),
        weight,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

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
        let (merged, conflicts) = merge_edges(&base, &ours, &theirs);
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
        let (merged, conflicts) = merge_edges(&base, &ours, &theirs);
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
        let (merged, _) = merge_edges(&base, &ours, &theirs);
        assert_eq!(merged.len(), 1);
        assert!((merged[0].weight - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn dangling_edge_detected() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let edges = vec![edge(a, b, 1.0)];
        // entity set only contains `a`, not `b`
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
        let ours = archive(vec![]); // deleted
        let theirs = archive(vec![edge(a, b, 1.0)]); // modified weight

        let (_, conflicts) = merge_edges(&base, &ours, &theirs);
        assert_eq!(conflicts.len(), 1);
        assert!(matches!(
            conflicts[0],
            MergeConflict::EdgeModifyDelete { .. }
        ));
    }
}
