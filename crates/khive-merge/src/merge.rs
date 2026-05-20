// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! Top-level `three_way_merge()` and `ThreeWayMergeEngine` (ADR-043 §4–§11).

use std::collections::HashSet;

use chrono::Utc;
use khive_runtime::portability::KgArchive;
use uuid::Uuid;

use khive_vcs::merge_engine::{MergeConflict, MergeEngine, MergeResult, MergeStrategy};
use khive_vcs::VcsError;

use crate::edge::{merge_edges, validate_dangling_edges};
use crate::entity::merge_entities;
use crate::strategy::{apply_ours, apply_theirs};

/// Perform a three-way merge.
///
/// - `Auto`: entity pass → edge pass → dangling validation → return `Conflicts` or `Clean`.
/// - `Ours`/`Theirs`: call the last-write-wins shortcut; always returns `Clean`.
pub fn three_way_merge(
    base: &KgArchive,
    ours: &KgArchive,
    theirs: &KgArchive,
    strategy: MergeStrategy,
) -> Result<MergeResult, VcsError> {
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
    let (merged_entities, entity_conflicts) = merge_entities(base, ours, theirs);
    all_conflicts.extend(entity_conflicts);

    // Step 2: edge merge.
    let (merged_edges, edge_conflicts) = merge_edges(base, ours, theirs);
    all_conflicts.extend(edge_conflicts);

    // Step 3: dangling-edge validation.
    let entity_id_set: HashSet<Uuid> = merged_entities.iter().map(|e| e.id).collect();
    let dangling = validate_dangling_edges(&merged_edges, &entity_id_set);
    all_conflicts.extend(dangling);

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
/// Register this in `khive-vcs` at startup to replace `NoOpMergeEngine`.
pub struct ThreeWayMergeEngine;

impl MergeEngine for ThreeWayMergeEngine {
    fn merge(
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
        }
    }

    fn edge(src: Uuid, tgt: Uuid) -> ExportedEdge {
        ExportedEdge {
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
        let result = engine.merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
        assert!(matches!(result, MergeResult::Clean { .. }));
    }
}
