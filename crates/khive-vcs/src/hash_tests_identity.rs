#[test]
fn empty_archive_has_stable_hash() {
    let a1 = empty_archive();
    let a2 = empty_archive();
    // Two empty archives produce the same hash regardless of `exported_at`.
    assert_eq!(
        snapshot_id_for_archive(&a1).unwrap(),
        snapshot_id_for_archive(&a2).unwrap()
    );
}

#[test]
fn hash_changes_with_entity_addition() {
    let mut archive = empty_archive();
    let id1 = snapshot_id_for_archive(&archive).unwrap();
    archive
        .entities
        .push(make_entity(Uuid::new_v4(), "FlashAttention"));
    let id2 = snapshot_id_for_archive(&archive).unwrap();
    assert_ne!(id1, id2);
}

#[test]
fn entity_order_independent_hash() {
    let e1 = make_entity(
        Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
        "Alpha",
    );
    let e2 = make_entity(
        Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
        "Beta",
    );

    let mut a1 = empty_archive();
    a1.entities = vec![e1.clone(), e2.clone()];
    let mut a2 = empty_archive();
    a2.entities = vec![e2, e1]; // reversed insertion order

    // Sort-by-UUID makes both hashes identical.
    assert_eq!(
        snapshot_id_for_archive(&a1).unwrap(),
        snapshot_id_for_archive(&a2).unwrap()
    );
}

#[test]
fn snapshot_id_has_sha256_prefix() {
    let id = snapshot_id_for_archive(&empty_archive()).unwrap();
    assert!(id.as_str().starts_with("sha256:"));
    assert_eq!(id.hex().len(), 64);
}

#[test]
fn edge_included_in_hash() {
    let uid1 = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
    let uid2 = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
    let e1 = make_entity(uid1, "A");
    let e2 = make_entity(uid2, "B");

    let mut without_edge = empty_archive();
    without_edge.entities = vec![e1.clone(), e2.clone()];

    let mut with_edge = empty_archive();
    with_edge.entities = vec![e1, e2];
    with_edge.edges = vec![ExportedEdge {
        edge_id: Uuid::new_v4(),
        source: uid1,
        target: uid2,
        relation: EdgeRelation::Extends,
        weight: 1.0,
        properties: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }];

    assert_ne!(
        snapshot_id_for_archive(&without_edge).unwrap(),
        snapshot_id_for_archive(&with_edge).unwrap()
    );
}

#[test]
fn symmetric_edge_direction_does_not_change_snapshot_id() {
    let low = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
    let high = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
    let edge_id = Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap();
    let timestamp = Utc::now();
    for relation in [EdgeRelation::CompetesWith, EdgeRelation::ComposedWith] {
        let edge = ExportedEdge {
            edge_id,
            source: low,
            target: high,
            relation,
            weight: 0.7,
            properties: None,
            created_at: timestamp,
            updated_at: timestamp,
        };
        let mut forward = empty_archive();
        forward.edges.push(edge.clone());
        let mut reversed = empty_archive();
        reversed.edges.push(ExportedEdge {
            source: high,
            target: low,
            ..edge
        });
        assert_eq!(
            snapshot_id_for_archive(&forward).unwrap(),
            snapshot_id_for_archive(&reversed).unwrap(),
            "{relation} endpoint order must not change snapshot identity"
        );
    }
}

#[test]
fn directed_edge_direction_still_changes_snapshot_id() {
    let low = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
    let high = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
    let edge = ExportedEdge {
        edge_id: Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap(),
        source: low,
        target: high,
        relation: EdgeRelation::Extends,
        weight: 0.7,
        properties: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let mut forward = empty_archive();
    forward.edges.push(edge.clone());
    let mut reversed = empty_archive();
    reversed.edges.push(ExportedEdge {
        source: high,
        target: low,
        ..edge
    });
    assert_ne!(
        snapshot_id_for_archive(&forward).unwrap(),
        snapshot_id_for_archive(&reversed).unwrap(),
        "directed edge orientation remains part of snapshot identity"
    );
}
