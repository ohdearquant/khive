use super::*;

fn four_subjects() -> (AnnBridge, [Uuid; 4]) {
    let ids = [1, 2, 3, 4].map(Uuid::from_u128);
    let vectors = vec![
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ];
    let bridge = AnnBridge::build(vectors, 4, ids.to_vec(), HashSet::new()).expect("build");
    (bridge, ids)
}

#[test]
fn replay_reuses_reverse_map_across_batches_and_recycled_slots() {
    let (mut bridge, [a, b, c, d]) = four_subjects();
    let e = Uuid::from_u128(5);
    assert!(
        bridge.reverse_map.is_none(),
        "build must leave the reverse map lazy"
    );

    bridge
        .apply_final_ops(vec![(a, None)], 1)
        .expect("delete first subject");
    bridge
        .apply_final_ops(vec![(e, Some(vec![1.0, 0.0, 0.0, 0.0])), (a, None)], 2)
        .expect("reuse slot and replay old deletion");
    bridge
        .apply_final_ops(vec![(b, Some(vec![0.0, 1.0, 1.0, 0.0]))], 3)
        .expect("update another subject");

    assert_eq!(
        bridge.reverse_map_builds, 1,
        "replay batches must reuse the reverse map"
    );
    let reverse = bridge.reverse_map.as_ref().expect("cached reverse map");
    assert_eq!(reverse.len(), 4);
    assert!(!reverse.contains_key(&a));
    for id in [b, c, d, e] {
        let ordinal = reverse[&id];
        assert_eq!(bridge.id_map[ordinal as usize], id);
        assert!(!bridge.index.is_tombstoned(ordinal));
    }
    assert_eq!(bridge.index.last_applied_seq(), Some(3));
    assert_eq!(
        bridge.dirty_ops, 0,
        "replay must leave dirty accounting to its caller"
    );
    assert_eq!(
        bridge.published_seq, 0,
        "replay must not advance the published watermark"
    );
    let hits = bridge.search(&[1.0, 0.0, 0.0, 0.0], 4).expect("search");
    assert!(hits.iter().any(|(id, score)| *id == e && *score > 0.9));
    assert!(!hits.iter().any(|(id, _)| *id == a));
}

#[test]
fn cached_reverse_map_forgets_previous_owner_before_later_replay() {
    let (mut bridge, [a, _, _, _]) = four_subjects();
    let e = Uuid::from_u128(5);
    bridge.apply_final_ops(vec![], 0).expect("initialize cache");
    // The lower-level lifecycle operation leaves the old external-id entry.
    bridge
        .index
        .tombstone(0)
        .expect("leave a stale cached owner");
    bridge
        .apply_final_ops(vec![(e, Some(vec![1.0, 0.0, 0.0, 0.0]))], 1)
        .expect("recycle the old owner's slot");
    let reverse = bridge.reverse_map.as_ref().unwrap();
    assert!(
        !reverse.contains_key(&a),
        "reused ordinal must forget its former owner"
    );
    assert_eq!(reverse[&e], 0);
    bridge
        .apply_final_ops(vec![(a, None)], 2)
        .expect("later old-owner deletion");
    assert!(
        !bridge.index.is_tombstoned(0),
        "old-owner replay must preserve the new live owner"
    );
    assert_eq!(bridge.id_map[0], e);
    assert_eq!(
        bridge.reverse_map_builds, 1,
        "recycled ownership must update the cached map in place"
    );
}

#[test]
fn consolidation_at_small_tau_remaps_uuids_and_cached_ordinals() {
    let (mut bridge, [a, b, c, d]) = four_subjects();
    bridge.set_applied_seq(10);
    bridge
        .apply_final_ops(vec![(b, None), (d, None)], 12)
        .expect("two deletions");
    bridge.dirty_ops = 2;
    assert!(!bridge.consolidate_if_needed(3).expect("below threshold"));
    assert!(
        bridge.consolidate_if_needed(2).expect("at threshold"),
        "small tau must consolidate accumulated churn"
    );
    assert_eq!(
        bridge.id_map,
        vec![a, c],
        "consolidation must apply the new-to-old UUID remap"
    );
    assert_eq!(bridge.index.num_vectors(), 2);
    assert_eq!(bridge.index.tombstone_count(), 0);
    assert_eq!(bridge.index.ops_since_consolidation(), 0);
    assert_eq!(
        bridge.reverse_map.as_ref().unwrap()[&c],
        1,
        "consolidation must refresh cached ordinals"
    );
    assert_eq!(bridge.reverse_map_builds, 2);
    assert_eq!(bridge.index.last_applied_seq(), Some(12));
    assert_eq!(bridge.published_seq, 10);
    assert_eq!(bridge.dirty_ops, 2);
    assert!(!bridge
        .consolidate_if_needed(2)
        .expect("reset consolidation counter"));
    let hits = bridge
        .search(&[0.0, 0.0, 1.0, 0.0], 2)
        .expect("search compacted graph");
    assert!(hits.iter().any(|(id, score)| *id == c && *score > 0.9));

    bridge
        .apply_final_ops(vec![(c, None)], 13)
        .expect("delete by remapped ordinal");
    assert!(bridge.index.is_tombstoned(1));
    assert_eq!(
        bridge.reverse_map_builds, 2,
        "post-consolidation replay must reuse the refreshed cache"
    );
    bridge.mark_checkpointed();
    assert_eq!(bridge.dirty_ops, 0);
    assert_eq!(bridge.published_seq, 13);
}

#[test]
fn consolidation_without_tombstones_preserves_uuid_ordinals() {
    let (mut bridge, [a, b, c, d]) = four_subjects();
    bridge
        .apply_final_ops(vec![(a, Some(vec![1.0, 1.0, 0.0, 0.0]))], 1)
        .expect("update reuses its deleted slot");
    assert_eq!(bridge.index.tombstone_count(), 0);
    assert!(bridge
        .consolidate_if_needed(2)
        .expect("consolidate insert-delete churn"));
    assert_eq!(
        bridge.id_map,
        vec![a, b, c, d],
        "empty consolidation remap must retain UUID ordinals"
    );
    assert_eq!(bridge.index.ops_since_consolidation(), 0);
    assert_eq!(bridge.reverse_map.as_ref().unwrap()[&d], 3);
    assert_eq!(bridge.index.last_applied_seq(), Some(1));
}

#[test]
fn loaded_bridge_initializes_reverse_map_only_on_first_replay() {
    let (mut bridge, [a, b, c, _]) = four_subjects();
    bridge.set_applied_seq(7);
    let dir = tempfile::tempdir().expect("segment directory");
    bridge.save_atomic(dir.path()).expect("persist bridge");
    let mut loaded = AnnBridge::load(dir.path()).expect("load bridge");
    assert!(
        loaded.reverse_map.is_none(),
        "loaded bridge must leave the reverse map lazy"
    );
    assert_eq!(loaded.published_seq, 7);
    assert_eq!(loaded.dirty_ops, 0);
    loaded
        .apply_final_ops(vec![(a, None)], 8)
        .expect("first loaded replay");
    loaded
        .apply_final_ops(vec![(b, None)], 9)
        .expect("second loaded replay");
    assert_eq!(
        loaded.reverse_map_builds, 1,
        "loaded replay must build the reverse map only once"
    );
    assert_eq!(loaded.reverse_map.as_ref().unwrap()[&c], 2);
    assert_eq!(
        loaded.published_seq, 7,
        "unpublished replay must retain its persisted baseline"
    );
    loaded
        .consolidate_if_needed(2)
        .expect("compact mapped bridge");
    loaded
        .save_atomic(dir.path())
        .expect("persist remapped IDs");
    loaded.mark_checkpointed();
    let reopened = AnnBridge::load(dir.path()).expect("reload consolidated bridge");
    assert_eq!(reopened.id_map, loaded.id_map);
    assert_eq!(reopened.published_seq, 9);
    assert_eq!(reopened.index.num_vectors(), 2);
}
