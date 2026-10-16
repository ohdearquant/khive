use chrono::Utc;
use khive_runtime::portability::{ExportedEdge, ExportedEntity, KgArchive};
use khive_storage::EdgeRelation;
use uuid::Uuid;

use super::*;

fn make_archive(entities: Vec<ExportedEntity>, edges: Vec<ExportedEdge>) -> KgArchive {
    KgArchive {
        format: "khive-kg".into(),
        version: "0.1".into(),
        namespace: "test".into(),
        exported_at: Utc::now(),
        entities,
        edges,
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

fn edge(src: Uuid, tgt: Uuid, weight: f64) -> ExportedEdge {
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

#[test]
fn unchanged_entity() {
    let id = Uuid::new_v4();
    let e = entity(id, "FlashAttention");
    let base = make_archive(vec![e.clone()], vec![]);
    let branch = make_archive(vec![e], vec![]);
    let diff = diff_entities(&base, &branch);
    assert!(matches!(diff[&id], EntityChange::Unchanged));
}

#[test]
fn added_entity() {
    let id = Uuid::new_v4();
    let base = make_archive(vec![], vec![]);
    let branch = make_archive(vec![entity(id, "New")], vec![]);
    let diff = diff_entities(&base, &branch);
    assert!(matches!(diff[&id], EntityChange::Added(_)));
}

#[test]
fn deleted_entity() {
    let id = Uuid::new_v4();
    let base = make_archive(vec![entity(id, "Old")], vec![]);
    let branch = make_archive(vec![], vec![]);
    let diff = diff_entities(&base, &branch);
    assert!(matches!(diff[&id], EntityChange::Deleted));
}

#[test]
fn modified_entity_name() {
    let id = Uuid::new_v4();
    let mut e2 = entity(id, "Original");
    let base = make_archive(vec![entity(id, "Original")], vec![]);
    e2.name = "Renamed".into();
    let branch = make_archive(vec![e2], vec![]);
    let diff = diff_entities(&base, &branch);
    assert!(matches!(diff[&id], EntityChange::Modified { .. }));
}

#[test]
fn unchanged_edge() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let e = edge(a, b, 1.0);
    let base = make_archive(vec![], vec![e.clone()]);
    let branch = make_archive(vec![], vec![e]);
    let diff = diff_edges(&base, &branch).unwrap();
    let key = EdgeKey {
        source: a,
        target: b,
        relation: "extends".into(),
    };
    assert!(matches!(diff[&key], EdgeChange::Unchanged));
}

#[test]
fn added_edge() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let base = make_archive(vec![], vec![]);
    let branch = make_archive(vec![], vec![edge(a, b, 0.8)]);
    let diff = diff_edges(&base, &branch).unwrap();
    let key = EdgeKey {
        source: a,
        target: b,
        relation: "extends".into(),
    };
    assert!(matches!(diff[&key], EdgeChange::Added(_)));
}

#[test]
fn weight_modified_edge() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let base = make_archive(vec![], vec![edge(a, b, 0.5)]);
    let branch = make_archive(vec![], vec![edge(a, b, 1.0)]);
    let diff = diff_edges(&base, &branch).unwrap();
    let key = EdgeKey {
        source: a,
        target: b,
        relation: "extends".into(),
    };
    assert!(matches!(diff[&key], EdgeChange::Modified { .. }));
}

#[test]
fn property_only_edge_change_is_modified_with_full_branch_record() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let base_edge = edge(a, b, 0.7);
    let mut branch_edge = base_edge.clone();
    branch_edge.properties = Some(serde_json::json!({"confidence": 0.95}));
    branch_edge.created_at = chrono::DateTime::parse_from_rfc3339("2026-03-03T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    branch_edge.updated_at = chrono::DateTime::parse_from_rfc3339("2026-04-04T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    let base = make_archive(vec![], vec![base_edge.clone()]);
    let branch = make_archive(vec![], vec![branch_edge.clone()]);
    let diff = diff_edges(&base, &branch).unwrap();
    let key = EdgeKey::from_edge(&base_edge);

    match &diff[&key] {
        EdgeChange::Modified { base, branch } => {
            assert_eq!(base.properties, None);
            assert_eq!(branch.edge_id, branch_edge.edge_id);
            assert_eq!(branch.properties, branch_edge.properties);
            assert_eq!(branch.created_at, branch_edge.created_at);
            assert_eq!(branch.updated_at, branch_edge.updated_at);
        }
        other => panic!("property-only change must be Modified, got {other:?}"),
    }
}

#[test]
fn edge_identity_change_is_modified() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let base_edge = edge(a, b, 0.7);
    let mut branch_edge = base_edge.clone();
    branch_edge.edge_id = Uuid::new_v4();

    let base = make_archive(vec![], vec![base_edge.clone()]);
    let branch = make_archive(vec![], vec![branch_edge]);
    let diff = diff_edges(&base, &branch).unwrap();

    assert!(matches!(
        diff[&EdgeKey::from_edge(&base_edge)],
        EdgeChange::Modified { .. }
    ));
}

#[test]
fn timestamp_only_edge_drift_is_unchanged() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let base_edge = edge(a, b, 0.7);
    let mut rebuilt_edge = base_edge.clone();
    rebuilt_edge.created_at += chrono::TimeDelta::seconds(1);
    rebuilt_edge.updated_at += chrono::TimeDelta::seconds(2);

    let base = make_archive(vec![], vec![base_edge.clone()]);
    let branch = make_archive(vec![], vec![rebuilt_edge]);
    let diff = diff_edges(&base, &branch).unwrap();

    assert!(matches!(
        diff[&EdgeKey::from_edge(&base_edge)],
        EdgeChange::Unchanged
    ));
}

#[test]
fn symmetric_edge_key_ignores_endpoint_order() {
    let low = Uuid::from_u128(1);
    let high = Uuid::from_u128(2);
    for relation in [EdgeRelation::CompetesWith, EdgeRelation::ComposedWith] {
        let forward = ExportedEdge {
            relation,
            ..edge(low, high, 1.0)
        };
        let reversed = ExportedEdge {
            relation,
            ..edge(high, low, 1.0)
        };
        let key = EdgeKey::from_edge(&forward);
        assert_eq!(key, EdgeKey::from_edge(&reversed), "{relation}");
        assert_eq!((key.source, key.target), (low, high), "{relation}");
    }
}
