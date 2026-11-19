use super::*;
use crate::parse_request;

#[test]
fn no_conflict_on_non_write_ops() {
    let r = parse_request(r#"[list(kind="entity"), search(kind="entity", query="x")]"#).unwrap();
    check_write_key_conflicts(&r).unwrap();
}

#[test]
fn update_and_delete_same_id_conflict() {
    let r = parse_request(r#"[update(id="abc-123", name="new"), delete(id="abc-123")]"#).unwrap();
    let err = check_write_key_conflicts(&r).unwrap_err();
    assert!(
        matches!(&err, DslError::WriteKeyConflict { id, first_op, second_op, .. }
                if id == "entity:abc-123" && first_op == "update" && second_op == "delete"),
        "expected WriteKeyConflict with entity-prefixed key, got {err:?}"
    );
}

#[test]
fn two_updates_same_id_conflict() {
    let r =
        parse_request(r#"[update(id="uuid-1", name="a"), update(id="uuid-1", description="b")]"#)
            .unwrap();
    let err = check_write_key_conflicts(&r).unwrap_err();
    assert!(
        matches!(&err, DslError::WriteKeyConflict { id, .. } if id == "entity:uuid-1"),
        "expected WriteKeyConflict with entity-prefixed key, got {err:?}"
    );
}

#[test]
fn merge_from_id_conflicts_with_delete() {
    let r = parse_request(r#"[merge(into_id="new-id", from_id="old-id"), delete(id="old-id")]"#)
        .unwrap();
    let err = check_write_key_conflicts(&r).unwrap_err();
    assert!(
        matches!(&err, DslError::WriteKeyConflict { id, .. } if id == "entity:old-id"),
        "expected WriteKeyConflict with entity-prefixed key, got {err:?}"
    );
}

#[test]
fn two_dry_run_merges_in_one_batch_do_not_conflict() {
    // The refusal this closes: two previews over the same pair were rejected as
    // overlapping writes and had to be issued as sequential singleton calls.
    let r = parse_request(
            r#"[merge(into_id="a-id", from_id="b-id", dry_run=true), merge(into_id="a-id", from_id="b-id", dry_run=true)]"#,
        )
        .unwrap();
    check_write_key_conflicts(&r).unwrap();
}

#[test]
fn a_dry_run_merge_does_not_conflict_with_a_delete_of_its_from_id() {
    // The same pair as merge_from_id_conflicts_with_delete above, which is the
    // control: the only difference between the two cases is the preview flag.
    let r = parse_request(
        r#"[merge(into_id="new-id", from_id="old-id", dry_run=true), delete(id="old-id")]"#,
    )
    .unwrap();
    check_write_key_conflicts(&r).unwrap();
}

#[test]
fn dry_run_false_keeps_the_merge_keys() {
    let r = parse_request(
        r#"[merge(into_id="new-id", from_id="old-id", dry_run=false), delete(id="old-id")]"#,
    )
    .unwrap();
    let err = check_write_key_conflicts(&r).unwrap_err();
    assert!(
        matches!(&err, DslError::WriteKeyConflict { id, .. } if id == "entity:old-id"),
        "a merge that writes must keep its keys, got {err:?}"
    );
}

#[test]
fn a_non_boolean_dry_run_keeps_the_merge_keys() {
    // A string is not the preview the handler acts on, so reading it as one here
    // would drop the keys of an op that goes on to write.
    let r = parse_request(
        r#"[merge(into_id="new-id", from_id="old-id", dry_run="true"), delete(id="old-id")]"#,
    )
    .unwrap();
    let err = check_write_key_conflicts(&r).unwrap_err();
    assert!(
        matches!(&err, DslError::WriteKeyConflict { id, .. } if id == "entity:old-id"),
        "a non-boolean dry_run must stay conservative, got {err:?}"
    );
}

#[test]
fn different_ids_no_conflict() {
    let r = parse_request(
        r#"[update(id="id-1", name="a"), delete(id="id-2"), update(id="id-3", name="c")]"#,
    )
    .unwrap();
    check_write_key_conflicts(&r).unwrap();
}

#[test]
fn chain_mode_skips_conflict_detection() {
    let r = parse_request(r#"update(id="same-id", name="a") | delete(id="same-id")"#).unwrap();
    assert_eq!(r.mode, ExecutionMode::Chain);
    check_write_key_conflicts(&r).unwrap();
}

#[test]
fn link_source_id_does_not_conflict_with_entity_update() {
    let r = parse_request(
            r#"[update(id="node-1", name="x"), link(source_id="node-1", target_id="node-2", relation="extends")]"#,
        )
        .unwrap();
    check_write_key_conflicts(&r).unwrap();
}

#[test]
fn two_links_same_natural_key_conflict() {
    let r = parse_request(
            r#"[link(source_id="a", target_id="b", relation="extends"), link(source_id="a", target_id="b", relation="extends")]"#,
        )
        .unwrap();
    let err = check_write_key_conflicts(&r).unwrap_err();
    assert!(
        matches!(&err, DslError::WriteKeyConflict { id, .. }
                if id == "edge-natural:a:b:extends"),
        "expected WriteKeyConflict on edge-natural key, got {err:?}"
    );
}

#[test]
fn mixed_spelling_links_to_one_edge_conflict_like_the_canonical_spelling() {
    // Both spellings of the same edge must produce the same key: the
    // canonical pair is the control, and each mixed ordering is the case a
    // key builder that only knows the canonical names lets through.
    for ops in [
        r#"[link(source_id="a", target_id="b", relation="extends", weight=0.1), link(source_id="a", target_id="b", relation="extends", weight=0.9)]"#,
        r#"[link(source_id="a", target_id="b", relation="extends", weight=0.1), link(source="a", target="b", kind="extends", weight=0.9)]"#,
        r#"[link(source="a", target="b", kind="extends", weight=0.1), link(source_id="a", target_id="b", relation="extends", weight=0.9)]"#,
    ] {
        let r = parse_request(ops).unwrap();
        let err = check_write_key_conflicts(&r).unwrap_err();
        assert!(
            matches!(&err, DslError::WriteKeyConflict { id, first_op, second_op, .. }
                    if id == "edge-natural:a:b:extends"
                        && first_op == "link"
                        && second_op == "link"),
            "{ops} must conflict on the edge key, got {err:?}"
        );
    }
}

#[test]
fn aliased_only_links_to_one_edge_conflict() {
    let r = parse_request(
            r#"[link(source="a", target="b", kind="extends"), link(source="a", target="b", kind="extends")]"#,
        )
        .unwrap();
    let err = check_write_key_conflicts(&r).unwrap_err();
    assert!(
        matches!(&err, DslError::WriteKeyConflict { id, .. }
                if id == "edge-natural:a:b:extends"),
        "expected WriteKeyConflict on edge-natural key, got {err:?}"
    );
}

#[test]
fn each_link_field_reads_its_own_alias_into_the_canonical_key() {
    // A directional relation, so an alias read into the wrong endpoint slot
    // would yield `b:a` rather than `a:b`.
    for ops in [
        r#"link(source="a", target="b", kind="extends")"#,
        r#"link(source="a", target_id="b", relation="extends")"#,
        r#"link(source_id="a", target="b", relation="extends")"#,
        r#"link(source_id="a", target_id="b", kind="extends")"#,
    ] {
        let r = parse_request(ops).unwrap();
        assert_eq!(
            write_keys_for_op_pub(&r.ops[0]),
            vec!["edge-natural:a:b:extends".to_string()],
            "{ops}"
        );
    }
}

#[test]
fn single_write_op_no_conflict() {
    let r = parse_request(r#"delete(id="solo-id")"#).unwrap();
    assert_eq!(r.mode, ExecutionMode::Single);
    check_write_key_conflicts(&r).unwrap();
}

#[test]
fn bulk_link_and_singleton_same_natural_key_conflict() {
    let r = parse_request(
            r#"[link(links=[{"source_id":"a","target_id":"b","relation":"extends","weight":0.1}]), link(source_id="a", target_id="b", relation="extends", weight=0.9)]"#,
        )
        .unwrap();
    let err = check_write_key_conflicts(&r).unwrap_err();
    assert!(
        matches!(&err, DslError::WriteKeyConflict { id, .. }
                if id == "edge-natural:a:b:extends"),
        "expected WriteKeyConflict on edge-natural key, got {err:?}"
    );
}

#[test]
fn reversed_symmetric_links_conflict() {
    let r = parse_request(
            r#"[link(source_id="b", target_id="a", relation="competes_with"), link(source_id="a", target_id="b", relation="competes_with")]"#,
        )
        .unwrap();
    let err = check_write_key_conflicts(&r).unwrap_err();
    assert!(
        matches!(&err, DslError::WriteKeyConflict { id, .. }
                if id == "edge-natural:a:b:competes_with"),
        "expected WriteKeyConflict on canonicalized symmetric key, got {err:?}"
    );
}

#[test]
fn reversed_non_symmetric_links_do_not_conflict() {
    for (relation, key) in [
        ("extends", "extends"),
        ("derivedfrom", "derived_from"),
        ("Custom-Relation", "Custom-Relation"),
    ] {
        let r = parse_request(&format!(
                r#"[link(source_id="b", target_id="a", relation="{relation}"), link(source_id="a", target_id="b", relation="{relation}")]"#,
            )).unwrap();
        check_write_key_conflicts(&r).unwrap();
        assert_eq!(
            write_keys_for_op_pub(&r.ops[0]),
            [format!("edge-natural:b:a:{key}")]
        );
    }
}

#[test]
fn write_keys_for_op_pub_bulk_link_extracts_all_edges() {
    let r = parse_request(
            r#"link(links=[{"source_id":"a","target_id":"b","relation":"extends"},{"source_id":"c","target_id":"d","relation":"extends"}])"#,
        )
        .unwrap();
    let keys = write_keys_for_op_pub(&r.ops[0]);
    assert_eq!(
        keys,
        vec![
            "edge-natural:a:b:extends".to_string(),
            "edge-natural:c:d:extends".to_string(),
        ]
    );
}

#[test]
fn unit_conflict_refuses_both_units_sharing_a_key() {
    let r = parse_request(
        r#"[update(id="x", name="1") | update(id="y", name="2"), update(id="x", name="3")]"#,
    )
    .unwrap();
    assert_eq!(r.ranges, vec![0..2, 2..3]);
    let conflicts = unit_write_key_conflicts(&r.ops, &r.ranges);
    assert_eq!(
        conflicts.len(),
        2,
        "both units must be named: {conflicts:?}"
    );
    assert_eq!(
        conflicts[&0],
        vec![UnitKeyConflict {
            key: "entity:x".to_string(),
            leaf: 0,
            other_unit: 1,
            other_leaf: 2,
        }]
    );
    assert_eq!(
        conflicts[&1],
        vec![UnitKeyConflict {
            key: "entity:x".to_string(),
            leaf: 2,
            other_unit: 0,
            other_leaf: 0,
        }]
    );
}

#[test]
fn same_key_repeated_within_one_unit_is_not_reported() {
    let r = parse_request(r#"[update(id="x", name="1") | update(id="x", name="2")]"#).unwrap();
    assert_eq!(r.ranges.len(), 1);
    assert_eq!(r.ranges[0], 0..2);
    let conflicts = unit_write_key_conflicts(&r.ops, &r.ranges);
    assert!(
        conflicts.is_empty(),
        "a chain's own ordered leaves must not conflict with each other: {conflicts:?}"
    );
}

#[test]
fn disjoint_units_share_no_conflict() {
    let r = parse_request(r#"[update(id="x", name="1"), update(id="y", name="2")]"#).unwrap();
    assert_eq!(r.ranges, vec![0..1, 1..2]);
    assert!(unit_write_key_conflicts(&r.ops, &r.ranges).is_empty());
}

#[test]
fn three_way_unit_conflict_names_every_affected_unit() {
    let r = parse_request(
        r#"[update(id="x", name="1"), update(id="x", name="2"), update(id="x", name="3")]"#,
    )
    .unwrap();
    let conflicts = unit_write_key_conflicts(&r.ops, &r.ranges);
    assert_eq!(conflicts.len(), 3, "{conflicts:?}");
}

#[test]
fn conflict_participants_are_direct_complete_and_sorted() {
    let req = parse_request(r#"[update(id="a"), merge(from_id="a", into_id="b"), delete(id="b"), update(id="c"), delete(id="c"), list(kind="entity")]"#).unwrap();
    assert_eq!(
        write_key_conflict_ops(&req.ops),
        vec![
            vec![0, 1],
            vec![0, 1, 2],
            vec![1, 2],
            vec![3, 4],
            vec![3, 4],
            vec![],
        ]
    );
    let req =
        parse_request(r#"[update(id="a"), list(kind="entity"), delete(id="a"), update(id="a")]"#)
            .unwrap();
    assert_eq!(
        write_key_conflict_ops(&req.ops),
        vec![vec![0, 2, 3], vec![], vec![0, 2, 3], vec![0, 2, 3]]
    );
    let error = check_write_key_conflicts(&req).unwrap_err();
    assert!(
        matches!(&error, DslError::WriteKeyConflict { conflict_ops, .. } if conflict_ops == &[0, 2, 3])
    );
    assert_eq!(error.to_string(), "write-key conflict: id \"entity:a\" is targeted by both \"update\" and \"delete\" in the same batch; split into separate requests");
}

#[test]
fn unit_participants_include_repeated_claims_but_not_collateral_leaves() {
    let req = parse_request(r#"[list(kind="entity") | update(id="a") | delete(id="a"), update(id="a"), delete(id="a"), list(kind="entity")]"#).unwrap();
    assert_eq!(
        unit_write_key_conflict_ops(&req.ops, &req.ranges),
        vec![
            vec![],
            vec![1, 2, 3, 4],
            vec![1, 2, 3, 4],
            vec![1, 2, 3, 4],
            vec![1, 2, 3, 4],
            vec![],
        ]
    );
    let req = parse_request(r#"[update(id="a") | update(id="b"), delete(id="a"), delete(id="b")]"#)
        .unwrap();
    assert_eq!(
        unit_write_key_conflict_ops(&req.ops, &req.ranges),
        vec![vec![0, 2], vec![1, 3], vec![0, 2], vec![1, 3]]
    );
    let req = parse_request(r#"[update(id="a") | delete(id="a"), update(id="b")]"#).unwrap();
    assert_eq!(
        unit_write_key_conflict_ops(&req.ops, &req.ranges),
        vec![Vec::<usize>::new(); 3]
    );
}

#[test]
fn conflict_diagnostics_preserve_flat_self_refusal_and_key_policy() {
    let req = parse_request(r#"merge(from_id="same", into_id="same")"#).unwrap();
    assert_eq!(write_key_conflict_ops(&req.ops), vec![vec![0]]);
    let req = parse_request(r#"[merge(from_id="a", into_id="b", dry_run=true), update(id="a"), link(links=[{"source_id":"x","target_id":"y","relation":"competes_with"},{"source_id":"x","target_id":"y","relation":"competes_with"}]), link(source="y",target="x",kind="competes-with")]"#).unwrap();
    assert_eq!(
        write_key_conflict_ops(&req.ops),
        vec![vec![], vec![], vec![2, 3], vec![2, 3]]
    );
}
