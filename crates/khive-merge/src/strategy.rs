// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! Last-write-wins shortcut strategies.
//!
//! See `crates/khive-merge/docs/api/three-way-merge.md` for composition rules.

use khive_runtime::portability::KgArchive;

use crate::types::MergeConflict;

/// Selects ours, plus additions unique to theirs, for entities and edges.
///
/// This helper does not validate or sort; [`crate::merge::three_way_merge`]
/// performs those steps and dangling-edge checks around it.
pub fn apply_ours(base: &KgArchive, ours: &KgArchive, theirs: &KgArchive) -> KgArchive {
    apply_ours_with_conflicts(base, ours, theirs).0
}

/// Keep the preferred edge when a new semantic key reuses its durable ID.
/// The top-level merge reports the skipped edge as a typed conflict.
pub(crate) fn apply_ours_with_conflicts(
    base: &KgArchive,
    ours: &KgArchive,
    theirs: &KgArchive,
) -> (KgArchive, Vec<MergeConflict>) {
    use crate::diff_local::EdgeKey;
    use khive_runtime::portability::{ExportedEdge, ExportedEntity};
    use std::collections::HashSet;
    use uuid::Uuid;

    let ours_ids: HashSet<Uuid> = ours.entities.iter().map(|e| e.id).collect();
    let base_ids: HashSet<Uuid> = base.entities.iter().map(|e| e.id).collect();

    let mut entities: Vec<ExportedEntity> = ours.entities.clone();
    for e in &theirs.entities {
        if !base_ids.contains(&e.id) && !ours_ids.contains(&e.id) {
            entities.push(e.clone());
        }
    }

    let ours_keys: HashSet<EdgeKey> = ours.edges.iter().map(EdgeKey::from_edge).collect();
    let base_keys: HashSet<EdgeKey> = base.edges.iter().map(EdgeKey::from_edge).collect();

    let mut edges: Vec<ExportedEdge> = ours.edges.clone();
    let mut used_edge_ids: HashSet<Uuid> = edges.iter().map(|edge| edge.edge_id).collect();
    let mut conflicts = Vec::new();
    for e in &theirs.edges {
        let key = EdgeKey::from_edge(e);
        if !base_keys.contains(&key) && !ours_keys.contains(&key) {
            if used_edge_ids.insert(e.edge_id) {
                edges.push(e.clone());
            } else {
                conflicts.push(MergeConflict::EdgeIdentityCollision {
                    source_id: e.source,
                    target_id: e.target,
                    relation: e.relation.to_string(),
                    attempted_edge_id: e.edge_id,
                    retained_edge_id: e.edge_id,
                });
            }
        }
    }

    let archive = KgArchive {
        format: ours.format.clone(),
        version: ours.version.clone(),
        namespace: ours.namespace.clone(),
        exported_at: ours.exported_at,
        entities,
        edges,
    };
    (archive, conflicts)
}

/// Selects theirs, plus additions unique to ours, by swapping [`apply_ours`].
pub fn apply_theirs(base: &KgArchive, ours: &KgArchive, theirs: &KgArchive) -> KgArchive {
    apply_ours(base, theirs, ours)
}

pub(crate) fn apply_theirs_with_conflicts(
    base: &KgArchive,
    ours: &KgArchive,
    theirs: &KgArchive,
) -> (KgArchive, Vec<MergeConflict>) {
    apply_ours_with_conflicts(base, theirs, ours)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use khive_runtime::portability::{ExportedEntity, KgArchive};
    use uuid::Uuid;

    use super::*;

    fn empty() -> KgArchive {
        KgArchive {
            format: "khive-kg".into(),
            version: "0.1".into(),
            namespace: "test".into(),
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

    #[test]
    fn apply_ours_uses_ours_version() {
        let id = Uuid::new_v4();
        let e_ours = entity(id, "OursName");
        let e_theirs = entity(id, "TheirsName");
        let mut base = empty();
        base.entities = vec![entity(id, "Original")];
        let mut ours = empty();
        ours.entities = vec![e_ours.clone()];
        let mut theirs = empty();
        theirs.entities = vec![e_theirs.clone()];

        let result = apply_ours(&base, &ours, &theirs);
        assert_eq!(result.entities.len(), 1);
        assert_eq!(result.entities[0].name, "OursName");
    }

    #[test]
    fn apply_theirs_uses_theirs_version() {
        let id = Uuid::new_v4();
        let mut base = empty();
        base.entities = vec![entity(id, "Original")];
        let mut ours = empty();
        ours.entities = vec![entity(id, "OursName")];
        let mut theirs = empty();
        theirs.entities = vec![entity(id, "TheirsName")];

        let result = apply_theirs(&base, &ours, &theirs);
        assert_eq!(result.entities.len(), 1);
        assert_eq!(result.entities[0].name, "TheirsName");
    }

    #[test]
    fn apply_ours_includes_theirs_only_additions() {
        let id_ours = Uuid::new_v4();
        let id_theirs = Uuid::new_v4();
        let base = empty();
        let mut ours = empty();
        ours.entities = vec![entity(id_ours, "A")];
        let mut theirs = empty();
        theirs.entities = vec![entity(id_theirs, "B")];

        let result = apply_ours(&base, &ours, &theirs);
        assert_eq!(result.entities.len(), 2);
    }
}
