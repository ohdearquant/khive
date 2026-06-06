// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! Integration tests for `three_way_merge()` and `ThreeWayMergeEngine`.

use chrono::Utc;
use khive_runtime::portability::{ExportedEdge, ExportedEntity, KgArchive};
use khive_storage::EdgeRelation;
use uuid::Uuid;

use khive_merge::types::{MergeConflict, MergeEngine, MergeError, MergeResult, MergeStrategy};
use khive_merge::{merge::three_way_merge, ThreeWayMergeEngine};

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
        entity_type: None,
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
        edge_id: Uuid::new_v4(),
        source: src,
        target: tgt,
        relation: EdgeRelation::Extends,
        weight: 1.0,
    }
}

fn edge_weighted(src: Uuid, tgt: Uuid, weight: f64) -> ExportedEdge {
    ExportedEdge {
        edge_id: Uuid::new_v4(),
        source: src,
        target: tgt,
        relation: EdgeRelation::Extends,
        weight,
    }
}

// ── Basic merge scenarios ───────────────────────────────────────────────────

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
fn dangling_edge_in_auto_merge() {
    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();

    let base = {
        let mut a = empty("test");
        a.entities = vec![entity(id1, "A")];
        a
    };
    let ours = {
        let mut a = empty("test");
        a.entities = vec![entity(id1, "A"), entity(id2, "B")];
        a.edges = vec![edge(id1, id2)];
        a
    };
    let theirs = {
        let mut a = empty("test");
        a.entities = vec![entity(id1, "A")];
        a
    };

    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
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
fn kind_conflict_detected() {
    let id = Uuid::new_v4();
    let base = {
        let mut a = empty("test");
        a.entities = vec![entity(id, "E")];
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

// ── KM-AUD-002: Duplicate-UUID additions are conflicts ──────────────────────

#[test]
fn duplicate_uuid_same_content_auto_resolves() {
    let id = Uuid::new_v4();
    let base = empty("test");
    let mut ours = empty("test");
    ours.entities = vec![entity(id, "Same")];
    let mut theirs = empty("test");
    theirs.entities = vec![entity(id, "Same")];

    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
    assert!(matches!(result, MergeResult::Clean { .. }));
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.entities.len(), 1);
        assert_eq!(merged.entities[0].name, "Same");
    }
}

#[test]
fn duplicate_uuid_different_content_is_conflict() {
    let id = Uuid::new_v4();
    let base = empty("test");
    let mut ours = empty("test");
    ours.entities = vec![entity(id, "NameA")];
    let mut theirs = empty("test");
    theirs.entities = vec![entity(id, "NameB")];

    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
    assert!(
        matches!(result, MergeResult::Conflicts { .. }),
        "duplicate UUID with different content must be a conflict"
    );
    if let MergeResult::Conflicts { conflicts } = result {
        assert!(
            conflicts
                .iter()
                .any(|c| matches!(c, MergeConflict::DuplicateAddition { .. })),
            "expected DuplicateAddition conflict, got: {conflicts:?}"
        );
    }
}

// ── KM-AUD-003: Deterministic output ────────────────────────────────────────

#[test]
fn repeated_auto_merge_produces_equal_output() {
    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();
    let base = empty("test");
    let mut ours = empty("test");
    ours.entities = vec![entity(id1, "A")];
    let mut theirs = empty("test");
    theirs.entities = vec![entity(id2, "B")];

    let r1 = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
    let r2 = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();

    if let (MergeResult::Clean { merged: m1 }, MergeResult::Clean { merged: m2 }) = (&r1, &r2) {
        assert_eq!(m1.exported_at, m2.exported_at, "timestamps must be equal");
        assert_eq!(m1.entities.len(), m2.entities.len());
        for (a, b) in m1.entities.iter().zip(m2.entities.iter()) {
            assert_eq!(a.id, b.id, "entity ordering must be deterministic");
        }
    } else {
        panic!("expected Clean results");
    }
}

#[test]
fn entities_sorted_by_uuid() {
    let mut ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
    let base = empty("test");
    let mut ours = empty("test");
    for &id in &ids {
        ours.entities.push(entity(id, &format!("E-{id}")));
    }
    let theirs = empty("test");

    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        ids.sort();
        let merged_ids: Vec<Uuid> = merged.entities.iter().map(|e| e.id).collect();
        assert_eq!(merged_ids, ids, "entities must be sorted by UUID");
    } else {
        panic!("expected Clean");
    }
}

#[test]
fn edges_sorted_by_source_target_relation() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();

    let base = empty("test");
    let mut ours = empty("test");
    ours.entities = vec![entity(a, "A"), entity(b, "B"), entity(c, "C")];
    // Add edges in reverse order to verify they get sorted.
    ours.edges = vec![edge(c, a), edge(b, c), edge(a, b)];
    let theirs = empty("test");

    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        for i in 1..merged.edges.len() {
            let prev = &merged.edges[i - 1];
            let curr = &merged.edges[i];
            let prev_key = (prev.source, prev.target, prev.relation.to_string());
            let curr_key = (curr.source, curr.target, curr.relation.to_string());
            assert!(
                prev_key <= curr_key,
                "edges must be sorted by (source, target, relation)"
            );
        }
    } else {
        panic!("expected Clean");
    }
}

#[test]
fn ours_strategy_output_is_sorted() {
    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();
    let base = empty("test");
    let mut ours = empty("test");
    // Put entities in reverse UUID order.
    if id1 > id2 {
        ours.entities = vec![entity(id1, "B"), entity(id2, "A")];
    } else {
        ours.entities = vec![entity(id2, "B"), entity(id1, "A")];
    }
    let theirs = empty("test");

    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Ours).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert!(merged.entities.len() >= 2);
        assert!(
            merged.entities[0].id < merged.entities[1].id,
            "ours strategy must sort entities by UUID"
        );
    } else {
        panic!("expected Clean");
    }
}

#[test]
fn theirs_strategy_output_is_sorted() {
    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();
    let base = empty("test");
    let ours = empty("test");
    let mut theirs = empty("test");
    if id1 > id2 {
        theirs.entities = vec![entity(id1, "B"), entity(id2, "A")];
    } else {
        theirs.entities = vec![entity(id2, "B"), entity(id1, "A")];
    }

    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Theirs).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert!(merged.entities.len() >= 2);
        assert!(
            merged.entities[0].id < merged.entities[1].id,
            "theirs strategy must sort entities by UUID"
        );
    } else {
        panic!("expected Clean");
    }
}

// ── KM-AUD-004: Input validation ────────────────────────────────────────────

#[test]
fn rejects_namespace_mismatch() {
    let base = empty("ns1");
    let ours = empty("ns2");
    let theirs = empty("ns1");

    let err = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap_err();
    assert!(
        matches!(err, MergeError::NamespaceMismatch { .. }),
        "expected NamespaceMismatch, got: {err:?}"
    );
}

#[test]
fn rejects_non_finite_edge_weight_nan() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let base = empty("test");
    let mut ours = empty("test");
    ours.edges = vec![edge_weighted(a, b, f64::NAN)];
    let theirs = empty("test");

    let err = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap_err();
    assert!(matches!(err, MergeError::InvalidEdgeWeight(_)));
}

#[test]
fn rejects_non_finite_edge_weight_inf() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let base = empty("test");
    let mut ours = empty("test");
    ours.edges = vec![edge_weighted(a, b, f64::INFINITY)];
    let theirs = empty("test");

    let err = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap_err();
    assert!(matches!(err, MergeError::InvalidEdgeWeight(_)));
}

#[test]
fn rejects_duplicate_entity_ids_in_archive() {
    let id = Uuid::new_v4();
    let base = empty("test");
    let mut ours = empty("test");
    ours.entities = vec![entity(id, "First"), entity(id, "Second")];
    let theirs = empty("test");

    let err = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap_err();
    assert!(
        matches!(err, MergeError::DuplicateEntityId { .. }),
        "expected DuplicateEntityId, got: {err:?}"
    );
}

#[test]
fn rejects_duplicate_edge_keys_in_archive() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let base = empty("test");
    let mut ours = empty("test");
    ours.edges = vec![edge(a, b), edge(a, b)];
    let theirs = empty("test");

    let err = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap_err();
    assert!(
        matches!(err, MergeError::DuplicateEdgeKey { .. }),
        "expected DuplicateEdgeKey, got: {err:?}"
    );
}

// ── Entity merge scenarios ──────────────────────────────────────────────────

#[test]
fn unchanged_entity_passes_through() {
    let id = Uuid::new_v4();
    let e = entity(id, "A");
    let base = archive_with_entities(vec![e.clone()]);
    let ours = archive_with_entities(vec![e.clone()]);
    let theirs = archive_with_entities(vec![e]);
    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.entities.len(), 1);
        assert_eq!(merged.entities[0].name, "A");
    } else {
        panic!("expected Clean");
    }
}

#[test]
fn added_in_ours_included() {
    let id = Uuid::new_v4();
    let base = archive_with_entities(vec![]);
    let ours = archive_with_entities(vec![entity(id, "New")]);
    let theirs = archive_with_entities(vec![]);
    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.entities.len(), 1);
    } else {
        panic!("expected Clean");
    }
}

#[test]
fn deleted_in_both_excluded() {
    let id = Uuid::new_v4();
    let base = archive_with_entities(vec![entity(id, "Old")]);
    let ours = archive_with_entities(vec![]);
    let theirs = archive_with_entities(vec![]);
    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.entities.len(), 0);
    } else {
        panic!("expected Clean");
    }
}

#[test]
fn modify_delete_conflict() {
    let id = Uuid::new_v4();
    let mut modified = entity(id, "Original");
    let base = archive_with_entities(vec![entity(id, "Original")]);
    modified.name = "Renamed".into();
    let ours = archive_with_entities(vec![modified]);
    let theirs = archive_with_entities(vec![]);

    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
    assert!(matches!(result, MergeResult::Conflicts { .. }));
    if let MergeResult::Conflicts { conflicts } = result {
        assert_eq!(conflicts.len(), 1);
        assert!(matches!(conflicts[0], MergeConflict::ModifyDelete { .. }));
    }
}

#[test]
fn property_mismatch_conflict() {
    let id = Uuid::new_v4();
    let mut e_ours = entity(id, "E");
    let mut e_theirs = entity(id, "E");
    e_ours.properties = Some(serde_json::json!({"year": "2023"}));
    e_theirs.properties = Some(serde_json::json!({"year": "2022"}));

    let base = archive_with_entities(vec![entity(id, "E")]);
    let ours = archive_with_entities(vec![e_ours]);
    let theirs = archive_with_entities(vec![e_theirs]);

    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
    assert!(matches!(result, MergeResult::Conflicts { .. }));
    if let MergeResult::Conflicts { conflicts } = result {
        assert!(conflicts
            .iter()
            .any(|c| matches!(c, MergeConflict::PropertyMismatch { .. })));
    }
}

#[test]
fn name_conflict_reported() {
    let id = Uuid::new_v4();
    let mut e_ours = entity(id, "OriginalName");
    let mut e_theirs = entity(id, "OriginalName");
    let base = archive_with_entities(vec![entity(id, "OriginalName")]);
    e_ours.name = "NameA".into();
    e_theirs.name = "NameB".into();

    let ours = archive_with_entities(vec![e_ours]);
    let theirs = archive_with_entities(vec![e_theirs]);

    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
    assert!(matches!(result, MergeResult::Conflicts { .. }));
    if let MergeResult::Conflicts { conflicts } = result {
        assert!(conflicts
            .iter()
            .any(|c| matches!(c, MergeConflict::NameConflict { .. })));
    }
}

#[test]
fn tags_are_unioned() {
    let id = Uuid::new_v4();
    let mut e_ours = entity(id, "E");
    let mut e_theirs = entity(id, "E");
    let base = archive_with_entities(vec![entity(id, "E")]);
    e_ours.tags = vec!["a".into(), "b".into()];
    e_theirs.tags = vec!["b".into(), "c".into()];

    let ours = archive_with_entities(vec![e_ours]);
    let theirs = archive_with_entities(vec![e_theirs]);

    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        let tags = &merged.entities[0].tags;
        assert!(tags.contains(&"a".to_string()));
        assert!(tags.contains(&"b".to_string()));
        assert!(tags.contains(&"c".to_string()));
    } else {
        panic!("expected Clean (tag conflicts not expected)");
    }
}

#[test]
fn theirs_only_property_keys_preserved() {
    let id = Uuid::new_v4();
    let mut e_ours = entity(id, "E");
    let mut e_theirs = entity(id, "E");
    let base = archive_with_entities(vec![entity(id, "E")]);
    e_ours.properties = Some(serde_json::json!({"year": "2023"}));
    e_theirs.properties = Some(serde_json::json!({"year": "2023", "author": "Smith"}));

    let ours = archive_with_entities(vec![e_ours]);
    let theirs = archive_with_entities(vec![e_theirs]);

    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        let props = merged.entities[0]
            .properties
            .as_ref()
            .expect("merged has properties");
        assert_eq!(props.get("year").and_then(|v| v.as_str()), Some("2023"));
        assert_eq!(
            props.get("author").and_then(|v| v.as_str()),
            Some("Smith"),
            "theirs-only key 'author' must be preserved in merged output"
        );
    } else {
        panic!("expected Clean");
    }
}

// ── Edge merge scenarios ────────────────────────────────────────────────────

#[test]
fn edge_added_in_ours_included() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let entities = vec![entity(a, "A"), entity(b, "B")];
    let base = archive_full(entities.clone(), vec![]);
    let ours = archive_full(entities.clone(), vec![edge(a, b)]);
    let theirs = archive_full(entities, vec![]);
    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.edges.len(), 1);
    } else {
        panic!("expected Clean");
    }
}

#[test]
fn edge_deleted_in_both_excluded() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let entities = vec![entity(a, "A"), entity(b, "B")];
    let base = archive_full(entities.clone(), vec![edge(a, b)]);
    let ours = archive_full(entities.clone(), vec![]);
    let theirs = archive_full(entities, vec![]);
    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.edges.len(), 0);
    } else {
        panic!("expected Clean");
    }
}

#[test]
fn max_weight_on_both_added() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let entities = vec![entity(a, "A"), entity(b, "B")];
    let base = archive_full(entities.clone(), vec![]);
    let ours = archive_full(entities.clone(), vec![edge_weighted(a, b, 0.6)]);
    let theirs = archive_full(entities, vec![edge_weighted(a, b, 0.9)]);
    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.edges.len(), 1);
        assert!((merged.edges[0].weight - 0.9).abs() < f64::EPSILON);
    } else {
        panic!("expected Clean");
    }
}

#[test]
fn dangling_edge_detected() {
    use std::collections::HashSet;
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let edges = vec![edge(a, b)];
    let entity_ids: HashSet<Uuid> = [a].into_iter().collect();
    let conflicts = khive_merge::edge::validate_dangling_edges(&edges, &entity_ids);
    assert_eq!(conflicts.len(), 1);
    assert!(
        matches!(&conflicts[0], MergeConflict::DanglingEdge { missing_endpoint, .. } if *missing_endpoint == b)
    );
}

#[test]
fn edge_modify_delete_conflict() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let entities = vec![entity(a, "A"), entity(b, "B")];
    let base = archive_full(entities.clone(), vec![edge_weighted(a, b, 0.5)]);
    let ours = archive_full(entities.clone(), vec![]);
    let theirs = archive_full(entities, vec![edge_weighted(a, b, 1.0)]);

    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
    assert!(matches!(result, MergeResult::Conflicts { .. }));
    if let MergeResult::Conflicts { conflicts } = result {
        assert!(
            conflicts
                .iter()
                .any(|c| matches!(c, MergeConflict::EdgeModifyDelete { .. })),
            "expected EdgeModifyDelete conflict, got: {conflicts:?}"
        );
    }
}

#[test]
fn merge_preserves_added_edge_id() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let branch_edge = edge(a, b);
    let expected_id = branch_edge.edge_id;
    let entities = vec![entity(a, "A"), entity(b, "B")];

    let base = archive_full(entities.clone(), vec![]);
    let ours = archive_full(entities.clone(), vec![branch_edge]);
    let theirs = archive_full(entities, vec![]);

    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.edges.len(), 1);
        assert_eq!(
            merged.edges[0].edge_id, expected_id,
            "merged edge_id must equal the branch's edge_id"
        );
    } else {
        panic!("expected Clean");
    }
}

#[test]
fn merge_preserves_weight_modified_edge_id() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();

    let base_edge = ExportedEdge {
        edge_id: Uuid::new_v4(),
        source: a,
        target: b,
        relation: EdgeRelation::Extends,
        weight: 0.5,
    };
    let ours_edge = ExportedEdge {
        edge_id: Uuid::new_v4(),
        source: a,
        target: b,
        relation: EdgeRelation::Extends,
        weight: 0.9,
    };
    let expected_id = ours_edge.edge_id;
    let entities = vec![entity(a, "A"), entity(b, "B")];

    let base = archive_full(entities.clone(), vec![base_edge.clone()]);
    let ours = archive_full(entities.clone(), vec![ours_edge]);
    let theirs = archive_full(entities, vec![base_edge]);

    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.edges.len(), 1);
        assert_eq!(merged.edges[0].weight, 0.9);
        assert_eq!(
            merged.edges[0].edge_id, expected_id,
            "merged edge_id must equal ours' edge_id after weight modification"
        );
    } else {
        panic!("expected Clean");
    }
}

// ── Strategy tests ──────────────────────────────────────────────────────────

#[test]
fn apply_ours_uses_ours_version() {
    let id = Uuid::new_v4();
    let mut base = empty("test");
    base.entities = vec![entity(id, "Original")];
    let mut ours = empty("test");
    ours.entities = vec![entity(id, "OursName")];
    let mut theirs = empty("test");
    theirs.entities = vec![entity(id, "TheirsName")];

    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Ours).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.entities.len(), 1);
        assert_eq!(merged.entities[0].name, "OursName");
    } else {
        panic!("expected Clean");
    }
}

#[test]
fn apply_theirs_uses_theirs_version() {
    let id = Uuid::new_v4();
    let mut base = empty("test");
    base.entities = vec![entity(id, "Original")];
    let mut ours = empty("test");
    ours.entities = vec![entity(id, "OursName")];
    let mut theirs = empty("test");
    theirs.entities = vec![entity(id, "TheirsName")];

    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Theirs).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.entities.len(), 1);
        assert_eq!(merged.entities[0].name, "TheirsName");
    } else {
        panic!("expected Clean");
    }
}

#[test]
fn apply_ours_includes_theirs_only_additions() {
    let id_ours = Uuid::new_v4();
    let id_theirs = Uuid::new_v4();
    let base = empty("test");
    let mut ours = empty("test");
    ours.entities = vec![entity(id_ours, "A")];
    let mut theirs = empty("test");
    theirs.entities = vec![entity(id_theirs, "B")];

    let result = three_way_merge(&base, &ours, &theirs, MergeStrategy::Ours).unwrap();
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.entities.len(), 2);
    } else {
        panic!("expected Clean");
    }
}

// ── Diff tests ──────────────────────────────────────────────────────────────

#[test]
fn diff_unchanged_entity() {
    use khive_merge::diff_local::{diff_entities, EntityChange};
    let id = Uuid::new_v4();
    let e = entity(id, "FlashAttention");
    let base = archive_with_entities(vec![e.clone()]);
    let branch = archive_with_entities(vec![e]);
    let diff = diff_entities(&base, &branch);
    assert!(matches!(diff[&id], EntityChange::Unchanged));
}

#[test]
fn diff_added_entity() {
    use khive_merge::diff_local::{diff_entities, EntityChange};
    let id = Uuid::new_v4();
    let base = archive_with_entities(vec![]);
    let branch = archive_with_entities(vec![entity(id, "New")]);
    let diff = diff_entities(&base, &branch);
    assert!(matches!(diff[&id], EntityChange::Added(_)));
}

#[test]
fn diff_deleted_entity() {
    use khive_merge::diff_local::{diff_entities, EntityChange};
    let id = Uuid::new_v4();
    let base = archive_with_entities(vec![entity(id, "Old")]);
    let branch = archive_with_entities(vec![]);
    let diff = diff_entities(&base, &branch);
    assert!(matches!(diff[&id], EntityChange::Deleted));
}

#[test]
fn diff_modified_entity_name() {
    use khive_merge::diff_local::{diff_entities, EntityChange};
    let id = Uuid::new_v4();
    let base = archive_with_entities(vec![entity(id, "Original")]);
    let mut e2 = entity(id, "Original");
    e2.name = "Renamed".into();
    let branch = archive_with_entities(vec![e2]);
    let diff = diff_entities(&base, &branch);
    assert!(matches!(diff[&id], EntityChange::Modified { .. }));
}

#[test]
fn diff_unchanged_edge() {
    use khive_merge::diff_local::{diff_edges, EdgeChange, EdgeKey};
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let e = edge(a, b);
    let base = archive_with_edges(vec![e.clone()]);
    let branch = archive_with_edges(vec![e]);
    let diff = diff_edges(&base, &branch).unwrap();
    let key = EdgeKey {
        source: a,
        target: b,
        relation: "extends".into(),
    };
    assert!(matches!(diff[&key], EdgeChange::Unchanged));
}

#[test]
fn diff_added_edge() {
    use khive_merge::diff_local::{diff_edges, EdgeChange, EdgeKey};
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let base = archive_with_edges(vec![]);
    let branch = archive_with_edges(vec![edge_weighted(a, b, 0.8)]);
    let diff = diff_edges(&base, &branch).unwrap();
    let key = EdgeKey {
        source: a,
        target: b,
        relation: "extends".into(),
    };
    assert!(matches!(diff[&key], EdgeChange::Added(_)));
}

#[test]
fn diff_weight_modified_edge() {
    use khive_merge::diff_local::{diff_edges, EdgeChange, EdgeKey};
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let base = archive_with_edges(vec![edge_weighted(a, b, 0.5)]);
    let branch = archive_with_edges(vec![edge_weighted(a, b, 1.0)]);
    let diff = diff_edges(&base, &branch).unwrap();
    let key = EdgeKey {
        source: a,
        target: b,
        relation: "extends".into(),
    };
    assert!(matches!(diff[&key], EdgeChange::WeightModified { .. }));
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn archive_with_entities(entities: Vec<ExportedEntity>) -> KgArchive {
    KgArchive {
        format: "khive-kg".into(),
        version: "0.1".into(),
        namespace: "test".into(),
        exported_at: Utc::now(),
        entities,
        edges: vec![],
    }
}

fn archive_with_edges(edges: Vec<ExportedEdge>) -> KgArchive {
    KgArchive {
        format: "khive-kg".into(),
        version: "0.1".into(),
        namespace: "test".into(),
        exported_at: Utc::now(),
        entities: vec![],
        edges,
    }
}

fn archive_full(entities: Vec<ExportedEntity>, edges: Vec<ExportedEdge>) -> KgArchive {
    KgArchive {
        format: "khive-kg".into(),
        version: "0.1".into(),
        namespace: "test".into(),
        exported_at: Utc::now(),
        entities,
        edges,
    }
}
