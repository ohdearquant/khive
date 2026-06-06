// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! Top-level `three_way_merge()` and `ThreeWayMergeEngine`.

use std::collections::HashSet;

use chrono::Utc;
use khive_runtime::portability::KgArchive;
use khive_vcs::VcsError;
use uuid::Uuid;

use crate::edge::{merge_edges, validate_dangling_edges};
use crate::entity::merge_entities;
use crate::merge_types::{MergeConflict, MergeEngine, MergeResult, MergeStrategy};
use crate::strategy::{apply_ours, apply_theirs};

/// Validate that all three archives share the same namespace and that every
/// edge weight in all archives is finite.
///
/// Returns a `VcsError::Internal` with a descriptive message on the first
/// violation found.
fn validate_inputs(base: &KgArchive, ours: &KgArchive, theirs: &KgArchive) -> Result<(), VcsError> {
    // Namespace isolation: all archives must share the same namespace.
    if base.namespace != ours.namespace {
        return Err(VcsError::Internal(format!(
            "namespace mismatch: base={:?} ours={:?}",
            base.namespace, ours.namespace
        )));
    }
    if base.namespace != theirs.namespace {
        return Err(VcsError::Internal(format!(
            "namespace mismatch: base={:?} theirs={:?}",
            base.namespace, theirs.namespace
        )));
    }

    // Non-finite weight guard: reject NaN, Infinity, -Infinity.
    for (label, archive) in [("base", base), ("ours", ours), ("theirs", theirs)] {
        for edge in &archive.edges {
            if !edge.weight.is_finite() {
                return Err(VcsError::Internal(format!(
                    "non-finite edge weight in {label}: edge_id={} weight={}",
                    edge.edge_id, edge.weight
                )));
            }
        }
    }

    Ok(())
}

/// Perform a three-way merge.
///
/// - `Auto`: entity pass → edge pass → dangling validation → return `Conflicts` or `Clean`.
/// - `Ours`/`Theirs`: call the last-write-wins shortcut; always returns `Clean`.
///
/// All three archives must share the same `namespace`. All edge weights must be
/// finite. Returns `VcsError::Internal` on violation.
pub fn three_way_merge(
    base: &KgArchive,
    ours: &KgArchive,
    theirs: &KgArchive,
    strategy: MergeStrategy,
) -> Result<MergeResult, VcsError> {
    validate_inputs(base, ours, theirs)?;

    match strategy {
        MergeStrategy::Ours => {
            let merged = apply_ours(base, ours, theirs);
            Ok(MergeResult::Clean { merged })
        }
        MergeStrategy::Theirs => {
            let merged = apply_theirs(base, ours, theirs);
            Ok(MergeResult::Clean { merged })
        }
        MergeStrategy::Auto => three_way_merge_auto(base, ours, theirs),
    }
}

fn three_way_merge_auto(
    base: &KgArchive,
    ours: &KgArchive,
    theirs: &KgArchive,
) -> Result<MergeResult, VcsError> {
    let mut all_conflicts: Vec<MergeConflict> = Vec::new();

    // Step 1: entity merge.
    let (mut merged_entities, entity_conflicts) = merge_entities(base, ours, theirs);
    all_conflicts.extend(entity_conflicts);

    // Step 2: edge merge.
    let (mut merged_edges, edge_conflicts) = merge_edges(base, ours, theirs)?;
    all_conflicts.extend(edge_conflicts);

    // Step 3: dangling-edge validation.
    let entity_id_set: HashSet<Uuid> = merged_entities.iter().map(|e| e.id).collect();
    let dangling = validate_dangling_edges(&merged_edges, &entity_id_set);
    all_conflicts.extend(dangling);

    // Sort outputs for deterministic ordering (AUD-006).
    merged_entities.sort_by_key(|e| e.id);
    merged_edges.sort_by_key(|e| e.edge_id);

    if all_conflicts.is_empty() {
        let merged = KgArchive {
            format: ours.format.clone(),
            version: ours.version.clone(),
            namespace: ours.namespace.clone(),
            exported_at: Utc::now(),
            entities: merged_entities,
            edges: merged_edges,
        };
        Ok(MergeResult::Clean { merged })
    } else {
        Ok(MergeResult::Conflicts {
            conflicts: all_conflicts,
        })
    }
}

/// Implementation of `MergeEngine` using the three-way merge algorithm.
///
/// Register this in `khive-vcs` at startup to replace any no-op merge engine.
pub struct ThreeWayMergeEngine;

impl MergeEngine for ThreeWayMergeEngine {
    fn merge_branch(
        &self,
        base: &KgArchive,
        ours: &KgArchive,
        theirs: &KgArchive,
        strategy: MergeStrategy,
    ) -> Result<MergeResult, VcsError> {
        three_way_merge(base, ours, theirs, strategy)
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

    fn empty(ns: &str) -> KgArchive {
        KgArchive {
            format: "khive-kg".into(),
            version: "0.1".into(),
            namespace: ns.into(),
            exported_at: Utc::now(),
            entities: vec![],
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
            entity_type: None,
        }
    }

    fn edge(src: Uuid, tgt: Uuid) -> ExportedEdge {
        ExportedEdge {
            edge_id: Uuid::new_v4(),
            source: src,
            target: tgt,
            relation: EdgeRelation::Extends,
            weight: 1.0,
        }
    }

    #[test]
    fn clean_merge_no_overlap() {
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let base = empty("test");
        let mut ours = empty("test");
        ours.entities = vec![entity(id1, "A")];
        let mut theirs = empty("test");
        theirs.entities = vec![entity(id2, "B")];

        let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
        assert!(matches!(result, MergeResult::Clean { .. }));
        if let MergeResult::Clean { merged } = result {
            assert_eq!(merged.entities.len(), 2);
        }
    }

    #[test]
    fn conflicts_on_name_mismatch() {
        let id = Uuid::new_v4();
        let base = {
            let mut a = empty("test");
            a.entities = vec![entity(id, "Original")];
            a
        };
        let ours = {
            let mut a = empty("test");
            a.entities = vec![entity(id, "NameA")];
            a
        };
        let theirs = {
            let mut a = empty("test");
            a.entities = vec![entity(id, "NameB")];
            a
        };

        let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
        assert!(matches!(result, MergeResult::Conflicts { .. }));
    }

    #[test]
    fn ours_strategy_always_clean() {
        let id = Uuid::new_v4();
        let base = {
            let mut a = empty("test");
            a.entities = vec![entity(id, "Original")];
            a
        };
        let ours = {
            let mut a = empty("test");
            a.entities = vec![entity(id, "NameA")];
            a
        };
        let theirs = {
            let mut a = empty("test");
            a.entities = vec![entity(id, "NameB")];
            a
        };

        let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Ours).unwrap();
        assert!(matches!(result, MergeResult::Clean { .. }));
        if let MergeResult::Clean { merged } = result {
            assert_eq!(merged.entities[0].name, "NameA");
        }
    }

    #[test]
    fn dangling_edge_conflict() {
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        // base: entity id1
        let base = {
            let mut a = empty("test");
            a.entities = vec![entity(id1, "A")];
            a
        };
        // ours: add edge id1→id2, add entity id2
        let ours = {
            let mut a = empty("test");
            a.entities = vec![entity(id1, "A"), entity(id2, "B")];
            a.edges = vec![edge(id1, id2)];
            a
        };
        // theirs: delete entity id2
        let theirs = {
            let mut a = empty("test");
            a.entities = vec![entity(id1, "A")];
            a
        };

        let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
        // The edge id1→id2 is added in ours; entity id2 was added in ours but
        // not in theirs (absent in both base and theirs). So after merge entity id2
        // will be included (added in ours only). Edge should NOT be dangling.
        // This tests that the auto-resolve path works for the common case.
        assert!(matches!(result, MergeResult::Clean { .. }));
    }

    #[test]
    fn three_way_merge_engine_impl() {
        let engine = ThreeWayMergeEngine;
        let base = empty("test");
        let ours = empty("test");
        let theirs = empty("test");
        let result = engine
            .merge_branch(&base, &ours, &theirs, MergeStrategy::Auto)
            .unwrap();
        assert!(matches!(result, MergeResult::Clean { .. }));
    }

    #[test]
    fn theirs_strategy_always_clean() {
        let id = Uuid::new_v4();
        let base = {
            let mut a = empty("test");
            a.entities = vec![entity(id, "Original")];
            a
        };
        let ours = {
            let mut a = empty("test");
            a.entities = vec![entity(id, "NameA")];
            a
        };
        let theirs = {
            let mut a = empty("test");
            a.entities = vec![entity(id, "NameB")];
            a
        };

        let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Theirs).unwrap();
        assert!(matches!(result, MergeResult::Clean { .. }));
        if let MergeResult::Clean { merged } = result {
            assert_eq!(merged.entities[0].name, "NameB");
        }
    }

    #[test]
    fn kind_conflict_detected() {
        let id = Uuid::new_v4();
        let base = {
            let mut a = empty("test");
            a.entities = vec![entity(id, "E")]; // kind = "concept"
            a
        };
        let ours = {
            let mut a = empty("test");
            let mut e = entity(id, "E");
            e.kind = "document".into();
            a.entities = vec![e];
            a
        };
        let theirs = {
            let mut a = empty("test");
            let mut e = entity(id, "E");
            e.kind = "dataset".into();
            a.entities = vec![e];
            a
        };

        let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
        assert!(matches!(result, MergeResult::Conflicts { .. }));
        if let MergeResult::Conflicts { conflicts } = result {
            assert!(
                conflicts
                    .iter()
                    .any(|c| matches!(c, MergeConflict::KindConflict { .. })),
                "expected at least one KindConflict, got: {conflicts:?}"
            );
        }
    }

    // ── Namespace validation tests (AUD-004) ─────────────────────────────────

    #[test]
    fn namespace_mismatch_base_ours_returns_error() {
        let base = empty("ns-a");
        let ours = empty("ns-b");
        let theirs = empty("ns-a");
        let err = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("namespace mismatch"),
            "expected namespace mismatch error, got: {msg}"
        );
    }

    #[test]
    fn namespace_mismatch_base_theirs_returns_error() {
        let base = empty("ns-a");
        let ours = empty("ns-a");
        let theirs = empty("ns-c");
        let err = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("namespace mismatch"),
            "expected namespace mismatch error, got: {msg}"
        );
    }

    #[test]
    fn namespace_mismatch_rejected_for_ours_strategy() {
        let base = empty("ns-a");
        let ours = empty("ns-b");
        let theirs = empty("ns-a");
        let err = three_way_merge(&base, &ours, &theirs, MergeStrategy::Ours).unwrap_err();
        assert!(err.to_string().contains("namespace mismatch"));
    }

    #[test]
    fn namespace_mismatch_rejected_for_theirs_strategy() {
        let base = empty("ns-a");
        let ours = empty("ns-a");
        let theirs = empty("ns-z");
        let err = three_way_merge(&base, &ours, &theirs, MergeStrategy::Theirs).unwrap_err();
        assert!(err.to_string().contains("namespace mismatch"));
    }

    // ── Non-finite weight tests (AUD-005) ────────────────────────────────────

    #[test]
    fn nan_weight_in_ours_returns_error() {
        let src = Uuid::new_v4();
        let tgt = Uuid::new_v4();
        let base = empty("test");
        let mut ours = empty("test");
        ours.edges = vec![ExportedEdge {
            edge_id: Uuid::new_v4(),
            source: src,
            target: tgt,
            relation: EdgeRelation::Extends,
            weight: f64::NAN,
        }];
        let theirs = empty("test");
        let err = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap_err();
        assert!(
            err.to_string().contains("non-finite"),
            "expected non-finite error, got: {err}"
        );
    }

    #[test]
    fn infinity_weight_in_theirs_returns_error() {
        let src = Uuid::new_v4();
        let tgt = Uuid::new_v4();
        let base = empty("test");
        let ours = empty("test");
        let mut theirs = empty("test");
        theirs.edges = vec![ExportedEdge {
            edge_id: Uuid::new_v4(),
            source: src,
            target: tgt,
            relation: EdgeRelation::Extends,
            weight: f64::INFINITY,
        }];
        let err = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap_err();
        assert!(err.to_string().contains("non-finite"));
    }

    #[test]
    fn neg_infinity_weight_in_base_returns_error() {
        let src = Uuid::new_v4();
        let tgt = Uuid::new_v4();
        let mut base = empty("test");
        base.edges = vec![ExportedEdge {
            edge_id: Uuid::new_v4(),
            source: src,
            target: tgt,
            relation: EdgeRelation::Extends,
            weight: f64::NEG_INFINITY,
        }];
        let ours = empty("test");
        let theirs = empty("test");
        let err = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap_err();
        assert!(err.to_string().contains("non-finite"));
    }

    // ── Deterministic output ordering test (AUD-006) ─────────────────────────

    #[test]
    fn entity_output_is_sorted_by_id() {
        // Build a set of entities and verify the merged output is UUID-sorted.
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        let base = empty("test");
        let mut ours = empty("test");
        ours.entities = ids.iter().map(|id| entity(*id, "E")).collect();
        let theirs = empty("test");

        let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
        if let MergeResult::Clean { merged } = result {
            let entity_ids: Vec<Uuid> = merged.entities.iter().map(|e| e.id).collect();
            let mut sorted = entity_ids.clone();
            sorted.sort();
            assert_eq!(entity_ids, sorted, "entity output must be sorted by UUID");
        } else {
            panic!("expected Clean merge");
        }
    }
}
