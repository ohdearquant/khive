// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! Minimal entity+edge diff computation for the merge use case (ADR-043 §3).
//!
//! This is a private implementation used only by `khive-merge`. It does NOT
//! implement the full `GraphDiff` format from ADR-017 — it produces the
//! categorized entity/edge change sets that the merge algorithm needs.
//!
//! When `khive-diff` ships in v0.4, this can be replaced by a dep on that crate.

use std::collections::{HashMap, HashSet};

use khive_runtime::portability::{ExportedEdge, ExportedEntity, KgArchive};
use uuid::Uuid;

/// Snapshot reader trait for `find_lca` (so the algorithm can be tested independently).
pub trait SnapshotReader: Send + Sync {
    fn parent_of(&self, id: &str) -> Option<String>;
}

/// Per-entity change classification between base and a branch.
#[derive(Debug, Clone)]
pub enum EntityChange {
    /// Present in base, unchanged in branch.
    Unchanged,
    /// Added in branch (absent in base).
    Added(ExportedEntity),
    /// Deleted in branch (present in base, absent in branch).
    Deleted,
    /// Modified in branch (fields differ from base).
    Modified {
        base: ExportedEntity,
        branch: ExportedEntity,
    },
}

/// Per-edge change classification.
#[derive(Debug, Clone)]
pub enum EdgeChange {
    /// Present in base, unchanged in branch.
    Unchanged,
    /// Added in branch.
    Added(ExportedEdge),
    /// Deleted in branch.
    Deleted,
    /// Weight modified.
    WeightModified {
        base_weight: f64,
        branch_weight: f64,
    },
}

/// Composite key for edge identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EdgeKey {
    pub source: Uuid,
    pub target: Uuid,
    pub relation: String,
}

impl EdgeKey {
    pub fn from_edge(e: &ExportedEdge) -> Self {
        Self {
            source: e.source,
            target: e.target,
            relation: e.relation.to_string(),
        }
    }
}

/// Compute entity changes between `base` and `branch`.
pub fn diff_entities(base: &KgArchive, branch: &KgArchive) -> HashMap<Uuid, EntityChange> {
    let base_map: HashMap<Uuid, &ExportedEntity> =
        base.entities.iter().map(|e| (e.id, e)).collect();
    let branch_map: HashMap<Uuid, &ExportedEntity> =
        branch.entities.iter().map(|e| (e.id, e)).collect();

    let all_ids: HashSet<Uuid> = base_map.keys().chain(branch_map.keys()).copied().collect();
    let mut result = HashMap::new();

    for id in all_ids {
        let change = match (base_map.get(&id), branch_map.get(&id)) {
            (None, Some(b)) => EntityChange::Added((*b).clone()),
            (Some(_), None) => EntityChange::Deleted,
            (Some(base_e), Some(branch_e)) => {
                if entities_equal(base_e, branch_e) {
                    EntityChange::Unchanged
                } else {
                    EntityChange::Modified {
                        base: (*base_e).clone(),
                        branch: (*branch_e).clone(),
                    }
                }
            }
            (None, None) => unreachable!(),
        };
        result.insert(id, change);
    }

    result
}

/// Compute edge changes between `base` and `branch`.
pub fn diff_edges(base: &KgArchive, branch: &KgArchive) -> HashMap<EdgeKey, EdgeChange> {
    let base_map: HashMap<EdgeKey, f64> = base
        .edges
        .iter()
        .map(|e| (EdgeKey::from_edge(e), e.weight))
        .collect();
    let branch_map: HashMap<EdgeKey, f64> = branch
        .edges
        .iter()
        .map(|e| (EdgeKey::from_edge(e), e.weight))
        .collect();

    let all_keys: HashSet<EdgeKey> = base_map.keys().chain(branch_map.keys()).cloned().collect();
    let mut result = HashMap::new();

    for key in all_keys {
        let change = match (base_map.get(&key), branch_map.get(&key)) {
            (None, Some(&w)) => EdgeChange::Added(ExportedEdge {
                source: key.source,
                target: key.target,
                relation: key.relation.parse().expect("valid relation"),
                weight: w,
            }),
            (Some(_), None) => EdgeChange::Deleted,
            (Some(&base_w), Some(&branch_w)) => {
                if (base_w - branch_w).abs() < f64::EPSILON {
                    EdgeChange::Unchanged
                } else {
                    EdgeChange::WeightModified {
                        base_weight: base_w,
                        branch_weight: branch_w,
                    }
                }
            }
            (None, None) => unreachable!(),
        };
        result.insert(key, change);
    }

    result
}

/// Structural equality check for entities (excludes timestamps).
fn entities_equal(a: &ExportedEntity, b: &ExportedEntity) -> bool {
    a.id == b.id
        && a.kind == b.kind
        && a.name == b.name
        && a.description == b.description
        && a.tags == b.tags
        && properties_equal(&a.properties, &b.properties)
}

fn properties_equal(a: &Option<serde_json::Value>, b: &Option<serde_json::Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(av), Some(bv)) => av == bv,
        _ => false,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use khive_runtime::portability::{ExportedEdge, ExportedEntity, KgArchive};
    use khive_storage::EdgeRelation;
    use uuid::Uuid;

    use super::*;

    fn make_archive(entities: Vec<ExportedEntity>, edges: Vec<ExportedEdge>) -> KgArchive {
        KgArchive {
            format: "khive-kg".into(),
            version: "0.1".into(),
            namespace: "test".into(),
            exported_at: Utc::now(),
            entities,
            edges,
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

    fn edge(src: Uuid, tgt: Uuid, weight: f64) -> ExportedEdge {
        ExportedEdge {
            source: src,
            target: tgt,
            relation: EdgeRelation::Extends,
            weight,
        }
    }

    #[test]
    fn unchanged_entity() {
        let id = Uuid::new_v4();
        let e = entity(id, "FlashAttention");
        let base = make_archive(vec![e.clone()], vec![]);
        let branch = make_archive(vec![e], vec![]);
        let diff = diff_entities(&base, &branch);
        assert!(matches!(diff[&id], EntityChange::Unchanged));
    }

    #[test]
    fn added_entity() {
        let id = Uuid::new_v4();
        let base = make_archive(vec![], vec![]);
        let branch = make_archive(vec![entity(id, "New")], vec![]);
        let diff = diff_entities(&base, &branch);
        assert!(matches!(diff[&id], EntityChange::Added(_)));
    }

    #[test]
    fn deleted_entity() {
        let id = Uuid::new_v4();
        let base = make_archive(vec![entity(id, "Old")], vec![]);
        let branch = make_archive(vec![], vec![]);
        let diff = diff_entities(&base, &branch);
        assert!(matches!(diff[&id], EntityChange::Deleted));
    }

    #[test]
    fn modified_entity_name() {
        let id = Uuid::new_v4();
        let mut e2 = entity(id, "Original");
        let base = make_archive(vec![entity(id, "Original")], vec![]);
        e2.name = "Renamed".into();
        let branch = make_archive(vec![e2], vec![]);
        let diff = diff_entities(&base, &branch);
        assert!(matches!(diff[&id], EntityChange::Modified { .. }));
    }

    #[test]
    fn unchanged_edge() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let e = edge(a, b, 1.0);
        let base = make_archive(vec![], vec![e.clone()]);
        let branch = make_archive(vec![], vec![e]);
        let diff = diff_edges(&base, &branch);
        let key = EdgeKey {
            source: a,
            target: b,
            relation: "extends".into(),
        };
        assert!(matches!(diff[&key], EdgeChange::Unchanged));
    }

    #[test]
    fn added_edge() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let base = make_archive(vec![], vec![]);
        let branch = make_archive(vec![], vec![edge(a, b, 0.8)]);
        let diff = diff_edges(&base, &branch);
        let key = EdgeKey {
            source: a,
            target: b,
            relation: "extends".into(),
        };
        assert!(matches!(diff[&key], EdgeChange::Added(_)));
    }

    #[test]
    fn weight_modified_edge() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let base = make_archive(vec![], vec![edge(a, b, 0.5)]);
        let branch = make_archive(vec![], vec![edge(a, b, 1.0)]);
        let diff = diff_edges(&base, &branch);
        let key = EdgeKey {
            source: a,
            target: b,
            relation: "extends".into(),
        };
        assert!(matches!(
            diff[&key],
            EdgeChange::WeightModified {
                base_weight: _,
                branch_weight: _
            }
        ));
    }
}
