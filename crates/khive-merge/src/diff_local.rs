// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! Merge-specific entity and edge change classification.
//!
//! See `crates/khive-merge/docs/api/entity-merge.md` and `edge-merge.md`.

use std::collections::{HashMap, HashSet};

use khive_runtime::portability::{ExportedEdge, ExportedEntity, KgArchive};
use uuid::Uuid;

use crate::types::MergeError;

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
        // Retained for future “was → now” conflict displays.
        #[allow(dead_code)]
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
    /// Merge-relevant edge content or durable identity modified.
    Modified {
        // Complete records are retained so a one-sided change can pass through
        // without losing edge identity, properties, or provenance timestamps.
        base: ExportedEdge,
        branch: ExportedEdge,
    },
}

/// Composite key for edge identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EdgeKey {
    /// Canonical source UUID.
    pub source: Uuid,
    /// Canonical target UUID.
    pub target: Uuid,
    /// Governed relation name.
    pub relation: String,
}

impl EdgeKey {
    /// Clones an edge's semantic identity into a key.
    ///
    /// Symmetric-relation endpoints are canonicalized to `(min, max)` so
    /// swapped duplicates compare equal.
    /// See `crates/khive-merge/docs/api/edge-merge.md` for identity rules.
    pub fn from_edge(e: &ExportedEdge) -> Self {
        let (source, target) = if e.relation.is_symmetric() && e.target < e.source {
            (e.target, e.source)
        } else {
            (e.source, e.target)
        };

        Self {
            source,
            target,
            relation: e.relation.to_string(),
        }
    }
}

/// Classifies every entity UUID in the union of `base` and `branch`.
///
/// Structural equality excludes timestamps. See
/// `crates/khive-merge/docs/api/entity-merge.md` for the classification table.
pub fn diff_entities(base: &KgArchive, branch: &KgArchive) -> HashMap<Uuid, EntityChange> {
    let base_map: HashMap<Uuid, &ExportedEntity> =
        base.entities.iter().map(|e| (e.id, e)).collect();
    let branch_map: HashMap<Uuid, &ExportedEntity> =
        branch.entities.iter().map(|e| (e.id, e)).collect();

    let all_ids: HashSet<Uuid> = base_map.keys().chain(branch_map.keys()).copied().collect();
    // Sort for deterministic output ordering (AUD-006).
    let mut all_ids_sorted: Vec<Uuid> = all_ids.into_iter().collect();
    all_ids_sorted.sort();
    let mut result = HashMap::new();

    for id in all_ids_sorted {
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

/// Classifies every semantic edge key in the union of `base` and `branch`.
///
/// Added and modified values retain complete edge records. Durable `edge_id`,
/// weight, and properties participate in classification; timestamps do not,
/// so deterministic archive rebuilds cannot manufacture semantic changes.
/// Weights differing by less than `f64::EPSILON` are unchanged. See
/// `crates/khive-merge/docs/api/edge-merge.md`.
///
/// # Errors
///
/// The current classifier is infallible; the result shape is retained for its
/// merge-layer contract.
pub fn diff_edges(
    base: &KgArchive,
    branch: &KgArchive,
) -> Result<HashMap<EdgeKey, EdgeChange>, MergeError> {
    let base_map: HashMap<EdgeKey, &ExportedEdge> = base
        .edges
        .iter()
        .map(|e| (EdgeKey::from_edge(e), e))
        .collect();
    let branch_map: HashMap<EdgeKey, &ExportedEdge> = branch
        .edges
        .iter()
        .map(|e| (EdgeKey::from_edge(e), e))
        .collect();

    let all_keys: HashSet<EdgeKey> = base_map.keys().chain(branch_map.keys()).cloned().collect();
    // Sort for deterministic output ordering (AUD-006).
    let mut all_keys_sorted: Vec<EdgeKey> = all_keys.into_iter().collect();
    all_keys_sorted.sort_by(|a, b| {
        a.source
            .cmp(&b.source)
            .then(a.target.cmp(&b.target))
            .then(a.relation.cmp(&b.relation))
    });
    let mut result = HashMap::new();

    for key in all_keys_sorted {
        let change = match (base_map.get(&key), branch_map.get(&key)) {
            (None, Some(branch_e)) => EdgeChange::Added((*branch_e).clone()),
            (Some(_), None) => EdgeChange::Deleted,
            (Some(base_e), Some(branch_e)) => {
                if edges_equal(base_e, branch_e) {
                    EdgeChange::Unchanged
                } else {
                    EdgeChange::Modified {
                        base: (*base_e).clone(),
                        branch: (*branch_e).clone(),
                    }
                }
            }
            (None, None) => unreachable!(),
        };
        result.insert(key, change);
    }

    Ok(result)
}

/// Compares merge-relevant fields for one semantic edge key.
///
/// Source, target, and relation form [`EdgeKey`] and are therefore classified
/// structurally as add/delete when they change. Timestamps are provenance, not
/// semantic content: a branch that wins a real change carries its complete
/// timestamps, while timestamp-only rebuild drift remains unchanged.
fn edges_equal(a: &ExportedEdge, b: &ExportedEdge) -> bool {
    a.edge_id == b.edge_id
        && (a.weight - b.weight).abs() < f64::EPSILON
        && properties_equal(&a.properties, &b.properties)
}

/// Compares merge-relevant entity fields, excluding timestamps.
fn entities_equal(a: &ExportedEntity, b: &ExportedEntity) -> bool {
    a.id == b.id
        && a.kind == b.kind
        && a.entity_type == b.entity_type
        && a.name == b.name
        && a.description == b.description
        && a.tags == b.tags
        && properties_equal(&a.properties, &b.properties)
}

/// Compares optional property payloads exactly.
pub(crate) fn properties_equal(
    a: &Option<serde_json::Value>,
    b: &Option<serde_json::Value>,
) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(av), Some(bv)) => av == bv,
        _ => false,
    }
}

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
            entity_type: None,
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
        let diff = diff_edges(&base, &branch).unwrap();
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
        let diff = diff_edges(&base, &branch).unwrap();
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
        let diff = diff_edges(&base, &branch).unwrap();
        let key = EdgeKey {
            source: a,
            target: b,
            relation: "extends".into(),
        };
        assert!(matches!(diff[&key], EdgeChange::Modified { .. }));
    }

    #[test]
    fn property_only_edge_change_is_modified_with_full_branch_record() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let base_edge = edge(a, b, 0.7);
        let mut branch_edge = base_edge.clone();
        branch_edge.properties = Some(serde_json::json!({"confidence": 0.95}));
        branch_edge.created_at = chrono::DateTime::parse_from_rfc3339("2026-03-03T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        branch_edge.updated_at = chrono::DateTime::parse_from_rfc3339("2026-04-04T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let base = make_archive(vec![], vec![base_edge.clone()]);
        let branch = make_archive(vec![], vec![branch_edge.clone()]);
        let diff = diff_edges(&base, &branch).unwrap();
        let key = EdgeKey::from_edge(&base_edge);

        match &diff[&key] {
            EdgeChange::Modified { base, branch } => {
                assert_eq!(base.properties, None);
                assert_eq!(branch.edge_id, branch_edge.edge_id);
                assert_eq!(branch.properties, branch_edge.properties);
                assert_eq!(branch.created_at, branch_edge.created_at);
                assert_eq!(branch.updated_at, branch_edge.updated_at);
            }
            other => panic!("property-only change must be Modified, got {other:?}"),
        }
    }

    #[test]
    fn edge_identity_change_is_modified() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let base_edge = edge(a, b, 0.7);
        let mut branch_edge = base_edge.clone();
        branch_edge.edge_id = Uuid::new_v4();

        let base = make_archive(vec![], vec![base_edge.clone()]);
        let branch = make_archive(vec![], vec![branch_edge]);
        let diff = diff_edges(&base, &branch).unwrap();

        assert!(matches!(
            diff[&EdgeKey::from_edge(&base_edge)],
            EdgeChange::Modified { .. }
        ));
    }

    #[test]
    fn timestamp_only_edge_drift_is_unchanged() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let base_edge = edge(a, b, 0.7);
        let mut rebuilt_edge = base_edge.clone();
        rebuilt_edge.created_at += chrono::TimeDelta::seconds(1);
        rebuilt_edge.updated_at += chrono::TimeDelta::seconds(2);

        let base = make_archive(vec![], vec![base_edge.clone()]);
        let branch = make_archive(vec![], vec![rebuilt_edge]);
        let diff = diff_edges(&base, &branch).unwrap();

        assert!(matches!(
            diff[&EdgeKey::from_edge(&base_edge)],
            EdgeChange::Unchanged
        ));
    }
}
