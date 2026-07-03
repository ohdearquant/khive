// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! Top-level `three_way_merge()` and `ThreeWayMergeEngine`.

use std::collections::HashSet;

use khive_runtime::portability::{ExportedEdge, KgArchive};
use uuid::Uuid;

use crate::diff_local::EdgeKey;
use crate::edge::{merge_edges, validate_dangling_edges};
use crate::entity::merge_entities;
use crate::strategy::{apply_ours, apply_theirs};
use crate::types::{MergeConflict, MergeEngine, MergeError, MergeResult, SnapshotMergeStrategy};

/// Validate archive invariants before merge.
///
/// Checks:
/// - All three archives share the same namespace.
/// - No edge has a non-finite weight (NaN / Inf).
/// - No duplicate entity IDs within a single archive.
/// - No duplicate edge natural keys `(source, target, relation)` within a single archive.
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
    // Reject non-finite edge weights.
    for edge in &archive.edges {
        if !edge.weight.is_finite() {
            return Err(MergeError::InvalidEdgeWeight(format!(
                "edge ({}, {}, {}): weight={}",
                edge.source, edge.target, edge.relation, edge.weight
            )));
        }
    }

    // Reject duplicate entity IDs.
    let mut entity_ids: HashSet<Uuid> = HashSet::with_capacity(archive.entities.len());
    for entity in &archive.entities {
        if !entity_ids.insert(entity.id) {
            return Err(MergeError::DuplicateEntityId {
                entity_id: entity.id,
            });
        }
    }

    // Reject duplicate edge IDs.
    let mut edge_ids: HashSet<Uuid> = HashSet::with_capacity(archive.edges.len());
    for edge in &archive.edges {
        if !edge_ids.insert(edge.edge_id) {
            return Err(MergeError::Internal(format!(
                "duplicate edge IDs in archive: {}",
                edge.edge_id
            )));
        }
    }

    // Reject duplicate edge semantic keys.
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

/// Sort entities by UUID for deterministic output.
fn sort_entities(archive: &mut KgArchive) {
    archive.entities.sort_by(|a, b| a.id.cmp(&b.id));
}

/// Sort edges by `(source, target, relation)` for deterministic output.
fn sort_edges(edges: &mut [ExportedEdge]) {
    edges.sort_by(|a, b| {
        a.source
            .cmp(&b.source)
            .then_with(|| a.target.cmp(&b.target))
            .then_with(|| a.relation.to_string().cmp(&b.relation.to_string()))
            .then_with(|| a.edge_id.cmp(&b.edge_id))
    });
}

/// Produce a deterministic `exported_at` timestamp: latest of `ours` and `theirs`.
fn deterministic_timestamp(ours: &KgArchive, theirs: &KgArchive) -> chrono::DateTime<chrono::Utc> {
    std::cmp::max(ours.exported_at, theirs.exported_at)
}

/// Perform a three-way merge.
///
/// - `Auto`: validate → entity pass → edge pass → dangling validation → deterministic sort → `Conflicts` or `Clean`.
/// - `Ours`/`Theirs`: validate → last-write-wins shortcut (skips field conflict detection) →
///   deterministic sort → dangling-edge validation against the shortcut-composed entity set,
///   returning `Conflicts` if the shortcut output would reference a missing endpoint, else `Clean`.
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

/// Finish a last-write-wins shortcut merge: sort, stamp, then check that the
/// shortcut-composed archive has no dangling edge endpoints before labeling
/// the result `Clean`.
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

/// Implementation of `MergeEngine` using the three-way merge algorithm.
///
/// Register this in `khive-vcs` at startup to replace `NoOpMergeEngine`.
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
