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
        // Used by three-way property reconciliation and future “was → now” displays.
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
        let (source, target) = e.relation.canonical_endpoints(e.source, e.target);

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
#[path = "diff_local_tests.rs"]
mod tests;
