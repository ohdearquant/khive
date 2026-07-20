//! Top-level merge orchestration and the concrete engine adapter.
//!
//! See `crates/khive-merge/docs/api/three-way-merge.md`.

use std::collections::HashSet;

use khive_runtime::portability::{ExportedEdge, KgArchive};
use uuid::Uuid;

use crate::diff_local::EdgeKey;
use crate::edge::{merge_edges, validate_dangling_edges};
use crate::entity::merge_entities;
use crate::strategy::{apply_ours, apply_theirs};
use crate::types::{MergeConflict, MergeEngine, MergeError, MergeResult, SnapshotMergeStrategy};

/// Validates namespace, finite-weight, and entity/edge uniqueness invariants.
fn validate_inputs(
    base: &KgArchive,
    ours: &KgArchive,
    theirs: &KgArchive,
) -> Result<(), MergeError> {
    if base.namespace != ours.namespace || base.namespace != theirs.namespace {
        return Err(MergeError::NamespaceMismatch {
            base: base.namespace.clone(),
            ours: ours.namespace.clone(),
            theirs: theirs.namespace.clone(),
        });
    }

    for archive in [base, ours, theirs] {
        validate_archive(archive)?;
    }

    Ok(())
}

fn validate_archive(archive: &KgArchive) -> Result<(), MergeError> {
    for edge in &archive.edges {
        if !edge.weight.is_finite() {
            return Err(MergeError::InvalidEdgeWeight(format!(
                "edge ({}, {}, {}): weight={}",
                edge.source, edge.target, edge.relation, edge.weight
            )));
        }
    }

    let mut entity_ids: HashSet<Uuid> = HashSet::with_capacity(archive.entities.len());
    for entity in &archive.entities {
        if !entity_ids.insert(entity.id) {
            return Err(MergeError::DuplicateEntityId {
                entity_id: entity.id,
            });
        }
    }

    let mut edge_ids: HashSet<Uuid> = HashSet::with_capacity(archive.edges.len());
    for edge in &archive.edges {
        if !edge_ids.insert(edge.edge_id) {
            return Err(MergeError::Internal(format!(
                "duplicate edge IDs in archive: {}",
                edge.edge_id
            )));
        }
    }

    let mut edge_keys: HashSet<EdgeKey> = HashSet::with_capacity(archive.edges.len());
    for edge in &archive.edges {
        let key = EdgeKey::from_edge(edge);
        if !edge_keys.insert(key.clone()) {
            return Err(MergeError::DuplicateEdgeKey {
                edge_source: key.source,
                edge_target: key.target,
                edge_relation: key.relation,
            });
        }
    }

    Ok(())
}

/// Sorts entities by UUID for deterministic output.
fn sort_entities(archive: &mut KgArchive) {
    archive.entities.sort_by_key(|entity| entity.id);
}

/// Sorts edges by semantic key and then edge UUID.
fn sort_edges(edges: &mut [ExportedEdge]) {
    edges.sort_by(|a, b| {
        a.source
            .cmp(&b.source)
            .then_with(|| a.target.cmp(&b.target))
            .then_with(|| a.relation.to_string().cmp(&b.relation.to_string()))
            .then_with(|| a.edge_id.cmp(&b.edge_id))
    });
}

/// Selects the later branch timestamp without reading the wall clock.
fn deterministic_timestamp(ours: &KgArchive, theirs: &KgArchive) -> chrono::DateTime<chrono::Utc> {
    std::cmp::max(ours.exported_at, theirs.exported_at)
}

/// Merges `ours` and `theirs` against their common `base` under `strategy`.
///
/// `Auto` performs entity and edge conflict detection; `Ours` and `Theirs`
/// apply last-write-wins but still validate dangling endpoints. Clean output is
/// deterministically sorted and stamped with the later branch timestamp.
///
/// # Errors
///
/// Returns [`MergeError`] for namespace mismatch, non-finite edge weights,
/// duplicate entity IDs, duplicate edge IDs or keys, or relation reconstruction.
/// See `crates/khive-merge/docs/api/three-way-merge.md` for the full pipeline.
pub fn three_way_merge(
    base: &KgArchive,
    ours: &KgArchive,
    theirs: &KgArchive,
    strategy: SnapshotMergeStrategy,
) -> Result<MergeResult, MergeError> {
    validate_inputs(base, ours, theirs)?;

    match strategy {
        SnapshotMergeStrategy::Ours => {
            let merged = apply_ours(base, ours, theirs);
            finish_shortcut_merge(merged, ours, theirs)
        }
        SnapshotMergeStrategy::Theirs => {
            let merged = apply_theirs(base, ours, theirs);
            finish_shortcut_merge(merged, ours, theirs)
        }
        SnapshotMergeStrategy::Auto => three_way_merge_auto(base, ours, theirs),
    }
}

/// Sorts, stamps, and dangling-checks a last-write-wins result.
fn finish_shortcut_merge(
    mut merged: KgArchive,
    ours: &KgArchive,
    theirs: &KgArchive,
) -> Result<MergeResult, MergeError> {
    sort_entities(&mut merged);
    sort_edges(&mut merged.edges);
    merged.exported_at = deterministic_timestamp(ours, theirs);

    let entity_id_set: HashSet<Uuid> = merged.entities.iter().map(|e| e.id).collect();
    let dangling = validate_dangling_edges(&merged.edges, &entity_id_set);
    if dangling.is_empty() {
        Ok(MergeResult::Clean { merged })
    } else {
        Ok(MergeResult::Conflicts {
            conflicts: dangling,
        })
    }
}

fn three_way_merge_auto(
    base: &KgArchive,
    ours: &KgArchive,
    theirs: &KgArchive,
) -> Result<MergeResult, MergeError> {
    let mut all_conflicts: Vec<MergeConflict> = Vec::new();

    let (merged_entities, entity_conflicts) = merge_entities(base, ours, theirs);
    all_conflicts.extend(entity_conflicts);

    let (merged_edges, edge_conflicts) = merge_edges(base, ours, theirs)?;
    all_conflicts.extend(edge_conflicts);

    let entity_id_set: HashSet<Uuid> = merged_entities.iter().map(|e| e.id).collect();
    let dangling = validate_dangling_edges(&merged_edges, &entity_id_set);
    all_conflicts.extend(dangling);

    if all_conflicts.is_empty() {
        let mut merged = KgArchive {
            format: ours.format.clone(),
            version: ours.version.clone(),
            namespace: ours.namespace.clone(),
            exported_at: deterministic_timestamp(ours, theirs),
            entities: merged_entities,
            edges: merged_edges,
        };
        sort_entities(&mut merged);
        sort_edges(&mut merged.edges);
        Ok(MergeResult::Clean { merged })
    } else {
        Ok(MergeResult::Conflicts {
            conflicts: all_conflicts,
        })
    }
}

/// Stateless [`MergeEngine`] adapter over [`three_way_merge`].
///
/// See `crates/khive-merge/docs/api/three-way-merge.md` for registration context.
pub struct ThreeWayMergeEngine;

impl MergeEngine for ThreeWayMergeEngine {
    fn merge_branch(
        &self,
        base: &KgArchive,
        ours: &KgArchive,
        theirs: &KgArchive,
        strategy: SnapshotMergeStrategy,
    ) -> Result<MergeResult, MergeError> {
        three_way_merge(base, ours, theirs, strategy)
    }
}
