// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! Integration tests for `three_way_merge()` and `ThreeWayMergeEngine`.

use chrono::Utc;
use khive_runtime::portability::{ExportedEdge, ExportedEntity, KgArchive};
use khive_storage::EdgeRelation;
use uuid::Uuid;

use khive_merge::types::{
    MergeConflict, MergeEngine, MergeError, MergeResult, SnapshotMergeStrategy,
};
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

fn entity_with_type(id: Uuid, name: &str, entity_type: Option<&str>) -> ExportedEntity {
    let mut e = entity(id, name);
    e.entity_type = entity_type.map(|s| s.to_string());
    e
}

fn edge(src: Uuid, tgt: Uuid) -> ExportedEdge {
    ExportedEdge {
        edge_id: Uuid::new_v4(),
        source: src,
        target: tgt,
        relation: EdgeRelation::Extends,
        weight: 1.0,
        properties: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

fn edge_weighted(src: Uuid, tgt: Uuid, weight: f64) -> ExportedEdge {
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

include!("three_way_merge_tests/basic_merge_tests.rs");

// ── KM-AUD-002: Duplicate-UUID additions are conflicts ──────────────────────

#[test]
fn duplicate_uuid_same_content_auto_resolves() {
    let id = Uuid::new_v4();
    let base = empty("test");
    let mut ours = empty("test");
    ours.entities = vec![entity(id, "Same")];
    let mut theirs = empty("test");
    theirs.entities = vec![entity(id, "Same")];

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
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

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
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

    let r1 = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    let r2 = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();

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

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
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

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
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

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Ours).unwrap();
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

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Theirs).unwrap();
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

    let err = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap_err();
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

    let err = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap_err();
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

    let err = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap_err();
    assert!(matches!(err, MergeError::InvalidEdgeWeight(_)));
}

#[test]
fn rejects_duplicate_entity_ids_in_archive() {
    let id = Uuid::new_v4();
    let base = empty("test");
    let mut ours = empty("test");
    ours.entities = vec![entity(id, "First"), entity(id, "Second")];
    let theirs = empty("test");

    let err = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap_err();
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

    let err = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap_err();
    assert!(
        matches!(err, MergeError::DuplicateEdgeKey { .. }),
        "expected DuplicateEdgeKey, got: {err:?}"
    );
}

include!("three_way_merge_tests/entity_merge_tests.rs");

include!("three_way_merge_tests/edge_merge_tests.rs");

fn divergent_property_archives() -> (KgArchive, KgArchive, KgArchive) {
    let a = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
    let b = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();
    let entities = vec![entity(a, "A"), entity(b, "B")];
    let mut base_edge = edge_weighted(a, b, 0.7);
    base_edge.properties = Some(serde_json::json!({"confidence": 0.5}));
    let mut ours_edge = base_edge.clone();
    ours_edge.properties = Some(serde_json::json!({"confidence": 0.8}));
    ours_edge.created_at = chrono::DateTime::parse_from_rfc3339("2026-05-05T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    ours_edge.updated_at = chrono::DateTime::parse_from_rfc3339("2026-06-06T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut theirs_edge = base_edge.clone();
    theirs_edge.properties = Some(serde_json::json!({"confidence": 0.9}));
    theirs_edge.created_at = chrono::DateTime::parse_from_rfc3339("2026-07-07T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    theirs_edge.updated_at = chrono::DateTime::parse_from_rfc3339("2026-08-08T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    (
        archive_full(entities.clone(), vec![base_edge]),
        archive_full(entities.clone(), vec![ours_edge]),
        archive_full(entities, vec![theirs_edge]),
    )
}

include!("three_way_merge_tests/edge_property_tests.rs");

include!("three_way_merge_tests/strategy_and_diff_tests.rs");

include!("three_way_merge_tests/identity_and_shortcut_tests.rs");

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
