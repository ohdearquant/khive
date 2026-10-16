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

    let mut conflicts: Vec<MergeConflict> = Vec::new();

    // Preserve the common-base record for unchanged output.
    let base_edge_map: HashMap<EdgeKey, &ExportedEdge> = base
        .edges
        .iter()
        .map(|e| (EdgeKey::from_edge(e), e))
        .collect();

    // First pass: decide each key's provisional edge per-key, same as before,
    // and note whether its durable identity is exactly base's (untouched) or
    // was chosen by branch content (added, or a modification that changed
    // `edge_id`). This classification drives collision resolution below,
    // independent of key iteration order.
    let mut candidates: Vec<(EdgeKey, Option<(ExportedEdge, bool)>)> =
        Vec::with_capacity(all_keys_sorted.len());

    for key in all_keys_sorted {
        let ours_change = ours_diff.get(&key);
        let theirs_change = theirs_diff.get(&key);

        let candidate = match (ours_change, theirs_change) {
            (Some(EdgeChange::Unchanged), Some(EdgeChange::Unchanged)) => {
                base_edge_map.get(&key).map(|&e| (e.clone(), true))
            }

            (Some(EdgeChange::Added(e)), None)
            | (Some(EdgeChange::Added(e)), Some(EdgeChange::Unchanged)) => Some((e.clone(), false)),

            (None, Some(EdgeChange::Added(e)))
            | (Some(EdgeChange::Unchanged), Some(EdgeChange::Added(e))) => Some((e.clone(), false)),

            (Some(EdgeChange::Added(e_ours)), Some(EdgeChange::Added(e_theirs))) => {
                let (edge, edge_conflicts) = merge_added_edges(&key, e_ours, e_theirs);
                conflicts.extend(edge_conflicts);
                Some((edge, false))
            }

            (Some(EdgeChange::Deleted), Some(EdgeChange::Deleted)) => None,

            (Some(EdgeChange::Deleted), Some(EdgeChange::Unchanged))
            | (Some(EdgeChange::Deleted), None) => None,

            (Some(EdgeChange::Unchanged), Some(EdgeChange::Deleted))
            | (None, Some(EdgeChange::Deleted)) => None,

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
                let identity_is_base = base_edge_map
                    .get(&key)
                    .is_some_and(|&b| b.edge_id == edge_ours.edge_id);
                Some((edge_ours.clone(), identity_is_base))
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
                let identity_is_base = base_edge_map
                    .get(&key)
                    .is_some_and(|&b| b.edge_id == edge_theirs.edge_id);
                Some((edge_theirs.clone(), identity_is_base))
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
                    merge_modified_edges(&key, base, edge_ours, edge_theirs);
                conflicts.extend(edge_conflicts);
                let identity_is_base = edge.edge_id == base.edge_id;
                Some((edge, identity_is_base))
            }

            (Some(EdgeChange::Deleted), Some(EdgeChange::Modified { .. })) => {
                conflicts.push(MergeConflict::EdgeModifyDelete {
                    source_id: key.source,
                    target_id: key.target,
                    relation: key.relation.clone(),
                    modified_in: BranchSide::Theirs,
                    deleted_in: BranchSide::Ours,
                });
                None
            }

            (Some(EdgeChange::Modified { .. }), Some(EdgeChange::Deleted)) => {
                conflicts.push(MergeConflict::EdgeModifyDelete {
                    source_id: key.source,
                    target_id: key.target,
                    relation: key.relation.clone(),
                    modified_in: BranchSide::Ours,
                    deleted_in: BranchSide::Theirs,
                });
                None
            }

            _ => None,
        };

        // Enforce the symmetric-endpoint invariant on every emitted record:
        // EdgeKey canonicalizes only the lookup key, so a branch record that
        // won a merge may still carry reversed endpoints.
        candidates.push((
            key,
            candidate.map(|(e, id_base)| (canonicalize_endpoints(e), id_base)),
        ));
    }

    // Second pass: reserve every durable identity inherited unchanged from
    // base before resolving branch-chosen identities, so a one-sided or
    // double edge modification can never displace an edge that kept its
    // pre-existing UUID — regardless of which key sorts first. Base edges are
    // validated duplicate-free on ingest, so these reservations never collide
    // with each other.
    let mut used_edge_ids: HashSet<Uuid> = HashSet::new();
    for (_, candidate) in &candidates {
        if let Some((edge, true)) = candidate {
            used_edge_ids.insert(edge.edge_id);
        }
    }

    // Third pass: resolve branch-chosen identities against the reserved set,
    // in the original sorted order, and emit the final deterministic output.
    let mut merged: Vec<ExportedEdge> = Vec::with_capacity(candidates.len());
    for (key, candidate) in candidates {
        let Some((mut edge, identity_is_base)) = candidate else {
            continue;
        };

        if !identity_is_base && !used_edge_ids.insert(edge.edge_id) {
            let attempted_edge_id = edge.edge_id;
            // The key's own base id is only a safe fallback if nothing else
            // has claimed it yet -- e.g. an earlier-sorted edge that
            // branch-chose this exact id. `insert`'s return value must be
            // checked, or a chained collision silently duplicates that
            // earlier edge's id instead of being caught here.
            let resolved_edge_id = base_edge_map
                .get(&key)
                .map(|&base_edge| base_edge.edge_id)
                .filter(|&id| used_edge_ids.insert(id));

            conflicts.push(MergeConflict::EdgeIdentityCollision {
                source_id: key.source,
                target_id: key.target,
                relation: key.relation.clone(),
                attempted_edge_id,
                retained_edge_id: resolved_edge_id.unwrap_or(attempted_edge_id),
            });

            match resolved_edge_id {
                Some(id) => edge.edge_id = id,
                // No unclaimed identity to restore -- dropping the edge is
                // the only way to keep the merged set duplicate-free; the
                // conflict entry above is the record of what was dropped.
                None => continue,
            }
        }

        merged.push(edge);
    }

    Ok((merged, conflicts))
}

/// Returns the record with symmetric-relation endpoints in canonical
/// `(min, max)` order, the same normalization [`EdgeKey`] applies to lookup
/// keys, so merged output always satisfies the storage endpoint invariant.
fn canonicalize_endpoints(mut e: ExportedEdge) -> ExportedEdge {
    (e.source, e.target) = e.relation.canonical_endpoints(e.source, e.target);
    e
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
#[path = "edge_tests.rs"]
mod tests;
