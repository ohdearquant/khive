// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! Integration tests for the public `three_way_merge()` and `ThreeWayMergeEngine`.

use chrono::Utc;
use khive_merge::{MergeConflict, MergeEngine, MergeResult, MergeStrategy, ThreeWayMergeEngine};
use khive_runtime::portability::{ExportedEdge, ExportedEntity, KgArchive};
use khive_storage::EdgeRelation;
use uuid::Uuid;

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

    let result =
        khive_merge::merge::three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
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

    let result =
        khive_merge::merge::three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
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

    let result =
        khive_merge::merge::three_way_merge(&base, &ours, &theirs, MergeStrategy::Ours).unwrap();
    assert!(matches!(result, MergeResult::Clean { .. }));
    if let MergeResult::Clean { merged } = result {
        assert_eq!(merged.entities[0].name, "NameA");
    }
}

#[test]
fn dangling_edge_conflict() {
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

    let result =
        khive_merge::merge::three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
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

    let result =
        khive_merge::merge::three_way_merge(&base, &ours, &theirs, MergeStrategy::Theirs).unwrap();
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

    let result =
        khive_merge::merge::three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
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

// ── Namespace validation tests ──────────────────────────────────────────────

#[test]
fn namespace_mismatch_base_ours_returns_error() {
    let base = empty("ns-a");
    let ours = empty("ns-b");
    let theirs = empty("ns-a");
    let err = khive_merge::merge::three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto)
        .unwrap_err();
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
    let err = khive_merge::merge::three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto)
        .unwrap_err();
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
    let err = khive_merge::merge::three_way_merge(&base, &ours, &theirs, MergeStrategy::Ours)
        .unwrap_err();
    assert!(err.to_string().contains("namespace mismatch"));
}

#[test]
fn namespace_mismatch_rejected_for_theirs_strategy() {
    let base = empty("ns-a");
    let ours = empty("ns-a");
    let theirs = empty("ns-z");
    let err = khive_merge::merge::three_way_merge(&base, &ours, &theirs, MergeStrategy::Theirs)
        .unwrap_err();
    assert!(err.to_string().contains("namespace mismatch"));
}

// ── Non-finite weight tests ─────────────────────────────────────────────────

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
    let err = khive_merge::merge::three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto)
        .unwrap_err();
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
    let err = khive_merge::merge::three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto)
        .unwrap_err();
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
    let err = khive_merge::merge::three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto)
        .unwrap_err();
    assert!(err.to_string().contains("non-finite"));
}

// ── Deterministic output ordering test ──────────────────────────────────────

#[test]
fn entity_output_is_sorted_by_id() {
    let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
    let base = empty("test");
    let mut ours = empty("test");
    ours.entities = ids.iter().map(|id| entity(*id, "E")).collect();
    let theirs = empty("test");

    let result =
        khive_merge::merge::three_way_merge(&base, &ours, &theirs, MergeStrategy::Auto).unwrap();
    if let MergeResult::Clean { merged } = result {
        let entity_ids: Vec<Uuid> = merged.entities.iter().map(|e| e.id).collect();
        let mut sorted = entity_ids.clone();
        sorted.sort();
        assert_eq!(entity_ids, sorted, "entity output must be sorted by UUID");
    } else {
        panic!("expected Clean merge");
    }
}
