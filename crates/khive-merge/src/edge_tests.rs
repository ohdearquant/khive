use chrono::Utc;
use khive_runtime::portability::{ExportedEdge, KgArchive};
use khive_storage::EdgeRelation;
use uuid::Uuid;

use super::*;

fn archive(edges: Vec<ExportedEdge>) -> KgArchive {
    KgArchive {
        format: "khive-kg".into(),
        version: "0.1".into(),
        namespace: "test".into(),
        exported_at: Utc::now(),
        entities: vec![],
        edges,
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
fn added_in_ours_included() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let base = archive(vec![]);
    let ours = archive(vec![edge(a, b, 1.0)]);
    let theirs = archive(vec![]);
    let (merged, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();
    assert!(conflicts.is_empty());
    assert_eq!(merged.len(), 1);
}

#[test]
fn deleted_in_both_excluded() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let base = archive(vec![edge(a, b, 1.0)]);
    let ours = archive(vec![]);
    let theirs = archive(vec![]);
    let (merged, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();
    assert!(conflicts.is_empty());
    assert_eq!(merged.len(), 0);
}

#[test]
fn max_weight_on_both_added() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let base = archive(vec![]);
    let ours = archive(vec![edge(a, b, 0.6)]);
    let theirs = archive(vec![edge(a, b, 0.9)]);
    let (merged, _) = merge_edges(&base, &ours, &theirs).unwrap();
    assert_eq!(merged.len(), 1);
    assert!((merged[0].weight - 0.9).abs() < f64::EPSILON);
}

#[test]
fn dangling_edge_detected() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let edges = vec![edge(a, b, 1.0)];
    let entity_ids: HashSet<Uuid> = [a].into_iter().collect();
    let conflicts = validate_dangling_edges(&edges, &entity_ids);
    assert_eq!(conflicts.len(), 1);
    assert!(
        matches!(conflicts[0], MergeConflict::DanglingEdge { missing_endpoint, .. } if missing_endpoint == b)
    );
}

#[test]
fn edge_modify_delete_conflict() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let base = archive(vec![edge(a, b, 0.5)]);
    let ours = archive(vec![]);
    let theirs = archive(vec![edge(a, b, 1.0)]);

    let (_, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();
    assert_eq!(conflicts.len(), 1);
    assert!(matches!(
        conflicts[0],
        MergeConflict::EdgeModifyDelete { .. }
    ));
}

#[test]
fn merge_preserves_added_edge_id() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let branch_edge = edge(a, b, 1.0);
    let expected_id = branch_edge.edge_id;

    let base = archive(vec![]);
    let ours = archive(vec![branch_edge]);
    let theirs = archive(vec![]);

    let (merged, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();
    assert!(conflicts.is_empty());
    assert_eq!(merged.len(), 1);
    assert_eq!(
        merged[0].edge_id, expected_id,
        "merged edge_id must equal the branch's edge_id, not a fresh UUID"
    );
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
        properties: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let ours_edge = ExportedEdge {
        edge_id: Uuid::new_v4(),
        source: a,
        target: b,
        relation: EdgeRelation::Extends,
        weight: 0.9,
        properties: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let expected_id = ours_edge.edge_id;

    let base = archive(vec![base_edge.clone()]);
    let ours = archive(vec![ours_edge]);
    let theirs = archive(vec![base_edge]);

    let (merged, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();
    assert!(conflicts.is_empty());
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].weight, 0.9);
    assert_eq!(
        merged[0].edge_id, expected_id,
        "merged edge_id must equal ours' edge_id after weight modification"
    );
}

#[test]
fn one_sided_property_change_preserves_complete_branch_edge() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let base_edge = edge(a, b, 0.7);
    let mut ours_edge = base_edge.clone();
    ours_edge.properties = Some(serde_json::json!({"confidence": 0.95}));
    ours_edge.created_at = chrono::DateTime::parse_from_rfc3339("2026-03-03T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    ours_edge.updated_at = chrono::DateTime::parse_from_rfc3339("2026-04-04T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    let base = archive(vec![base_edge.clone()]);
    let ours = archive(vec![ours_edge.clone()]);
    let theirs = archive(vec![base_edge]);
    let (merged, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();

    assert!(conflicts.is_empty(), "unexpected conflicts: {conflicts:?}");
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].edge_id, ours_edge.edge_id);
    assert_eq!(merged[0].properties, ours_edge.properties);
    assert_eq!(merged[0].created_at, ours_edge.created_at);
    assert_eq!(merged[0].updated_at, ours_edge.updated_at);
}

#[test]
fn divergent_property_only_changes_report_typed_conflict() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let mut base_edge = edge(a, b, 0.7);
    base_edge.properties = Some(serde_json::json!({"confidence": 0.5}));
    let mut ours_edge = base_edge.clone();
    ours_edge.properties = Some(serde_json::json!({"confidence": 0.8}));
    let mut theirs_edge = base_edge.clone();
    theirs_edge.properties = Some(serde_json::json!({"confidence": 0.9}));

    let base = archive(vec![base_edge]);
    let ours = archive(vec![ours_edge]);
    let theirs = archive(vec![theirs_edge]);
    let (_, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();

    assert!(matches!(
        conflicts.as_slice(),
        [MergeConflict::EdgePropertyMismatch {
            source_id,
            target_id,
            relation,
            ours: Some(ours),
            theirs: Some(theirs),
        }] if *source_id == a
            && *target_id == b
            && relation == "extends"
            && ours == &serde_json::json!({"confidence": 0.8})
            && theirs == &serde_json::json!({"confidence": 0.9})
    ));
}

#[test]
fn independent_property_key_changes_merge_without_loss() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let mut base_edge = edge(a, b, 0.7);
    base_edge.properties = Some(serde_json::json!({"shared": "base"}));
    let mut ours_edge = base_edge.clone();
    ours_edge.properties = Some(serde_json::json!({
        "ours": 1,
        "shared": "base",
    }));
    let mut theirs_edge = base_edge.clone();
    theirs_edge.properties = Some(serde_json::json!({
        "shared": "base",
        "theirs": 2,
    }));

    let base = archive(vec![base_edge]);
    let ours = archive(vec![ours_edge]);
    let theirs = archive(vec![theirs_edge]);
    let (merged, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();

    assert!(conflicts.is_empty(), "unexpected conflicts: {conflicts:?}");
    assert_eq!(
        merged[0].properties,
        Some(serde_json::json!({
            "ours": 1,
            "shared": "base",
            "theirs": 2,
        }))
    );
}

#[test]
fn independent_weight_and_property_changes_merge_without_loss() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let mut base_edge = edge(a, b, 0.5);
    base_edge.properties = Some(serde_json::json!({"origin": "base"}));
    let mut ours_edge = base_edge.clone();
    ours_edge.properties = Some(serde_json::json!({"origin": "ours"}));
    let mut theirs_edge = base_edge.clone();
    theirs_edge.weight = 0.2;

    let base = archive(vec![base_edge]);
    let ours = archive(vec![ours_edge.clone()]);
    let theirs = archive(vec![theirs_edge]);
    let (merged, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();

    assert!(conflicts.is_empty(), "unexpected conflicts: {conflicts:?}");
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].weight, 0.2);
    assert_eq!(merged[0].properties, ours_edge.properties);
    assert_eq!(merged[0].created_at, ours_edge.created_at);
    assert_eq!(merged[0].updated_at, ours_edge.updated_at);
}

#[test]
fn divergent_edge_identity_changes_report_typed_conflict() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let base_edge = edge(a, b, 0.7);
    let mut ours_edge = base_edge.clone();
    ours_edge.edge_id = Uuid::new_v4();
    let mut theirs_edge = base_edge.clone();
    theirs_edge.edge_id = Uuid::new_v4();

    let base = archive(vec![base_edge]);
    let ours = archive(vec![ours_edge.clone()]);
    let theirs = archive(vec![theirs_edge.clone()]);
    let (_, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();

    assert!(matches!(
        conflicts.as_slice(),
        [MergeConflict::EdgeIdentityMismatch {
            source_id,
            target_id,
            relation,
            ours,
            theirs,
        }] if *source_id == a
            && *target_id == b
            && relation == "extends"
            && *ours == ours_edge.edge_id
            && *theirs == theirs_edge.edge_id
    ));
}

#[test]
fn one_sided_identity_change_colliding_with_other_edge_is_rejected() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    let d = Uuid::new_v4();

    let edge1_base = edge(a, b, 1.0);
    let edge2 = edge(c, d, 1.0);

    let mut edge1_ours = edge1_base.clone();
    edge1_ours.edge_id = edge2.edge_id;

    let base = archive(vec![edge1_base.clone(), edge2.clone()]);
    let ours = archive(vec![edge1_ours, edge2.clone()]);
    let theirs = archive(vec![edge1_base, edge2.clone()]);

    let (merged, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();

    let mut seen_ids = HashSet::new();
    for merged_edge in &merged {
        assert!(
            seen_ids.insert(merged_edge.edge_id),
            "merge output must not contain duplicate durable edge_id {:?}: {merged:?}",
            merged_edge.edge_id
        );
    }
    assert!(
        conflicts
            .iter()
            .any(|c| matches!(c, MergeConflict::EdgeIdentityCollision { .. })),
        "expected an EdgeIdentityCollision conflict, got: {conflicts:?}"
    );
}

#[test]
fn chained_identity_collision_through_base_fallback_is_rejected() {
    // Three semantic edges, sorted by (source, target, relation) as
    // key1 < key_early < key_chained:
    //
    //   key1:        base-identity edge, reserves ID_B in the second pass.
    //   key_early:   a freshly added edge that branch-chooses ID_X, which
    //                happens to equal key_chained's *own* base edge_id.
    //                It claims ID_X first because it sorts before
    //                key_chained.
    //   key_chained: a modified edge whose branch-chosen id collides with
    //                ID_B (already reserved), so it falls back to its own
    //                base id -- ID_X -- but ID_X was just claimed by
    //                key_early. The fallback insert must be checked, or
    //                key_chained silently duplicates key_early's ID_X.
    let s1 = Uuid::from_u128(1);
    let t1 = Uuid::from_u128(2);
    let s2 = Uuid::from_u128(3);
    let t2 = Uuid::from_u128(4);
    let s3 = Uuid::from_u128(5);
    let t3 = Uuid::from_u128(6);

    let id_b = Uuid::from_u128(100);
    let id_x = Uuid::from_u128(200);

    let key1_base = edge(s1, t1, 1.0);
    let mut key1_ours = key1_base.clone();
    key1_ours.edge_id = id_b;
    let mut key1_base_fixed = key1_base.clone();
    key1_base_fixed.edge_id = id_b;
    let key1_base = key1_base_fixed;
    key1_ours.weight = 2.0;

    let key_chained_base = {
        let mut e = edge(s3, t3, 1.0);
        e.edge_id = id_x;
        e
    };
    let mut key_chained_ours = key_chained_base.clone();
    key_chained_ours.edge_id = id_b; // collides with key1's reserved id_b
    key_chained_ours.weight = 2.0;

    let key_early_ours = {
        let mut e = edge(s2, t2, 1.0);
        e.edge_id = id_x; // collides with key_chained's fallback target
        e
    };

    let base = archive(vec![key1_base.clone(), key_chained_base.clone()]);
    let ours = archive(vec![key1_ours, key_early_ours, key_chained_ours]);
    let theirs = archive(vec![key1_base, key_chained_base]);

    let (merged, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();

    let merged_rows: Vec<(Uuid, Uuid, Uuid, f64)> = merged
        .iter()
        .map(|e| (e.source, e.target, e.edge_id, e.weight))
        .collect();
    assert_eq!(
        merged_rows,
        vec![(s1, t1, id_b, 2.0), (s2, t2, id_x, 1.0)],
        "exactly key1 (reserved ID_B) and key_early (branch-chosen ID_X) \
             survive; key_chained is dropped rather than duplicating ID_X: \
             {merged:?}"
    );

    match conflicts.as_slice() {
        [MergeConflict::EdgeIdentityCollision {
            source_id,
            target_id,
            relation,
            attempted_edge_id,
            retained_edge_id,
        }] => {
            assert_eq!((*source_id, *target_id), (s3, t3));
            assert_eq!(relation, &EdgeRelation::Extends.to_string());
            assert_eq!(*attempted_edge_id, id_b);
            assert_eq!(
                *retained_edge_id, id_b,
                "a dropped edge records the attempted id as retained"
            );
        }
        other => panic!(
            "expected exactly one EdgeIdentityCollision for key_chained, \
                 got: {other:?}"
        ),
    }
}

#[test]
fn property_modify_delete_is_an_edge_modify_delete_conflict() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let base_edge = edge(a, b, 0.7);
    let mut theirs_edge = base_edge.clone();
    theirs_edge.properties = Some(serde_json::json!({"confidence": 0.95}));

    let base = archive(vec![base_edge]);
    let ours = archive(vec![]);
    let theirs = archive(vec![theirs_edge]);
    let (_, conflicts) = merge_edges(&base, &ours, &theirs).unwrap();

    assert!(matches!(
        conflicts.as_slice(),
        [MergeConflict::EdgeModifyDelete {
            modified_in: BranchSide::Theirs,
            deleted_in: BranchSide::Ours,
            ..
        }]
    ));
}
