// ── ADR-221 section 4: entity keys through the three-way merge ─────────────

fn keyed(id: Uuid, name: &str, kind: &str, keys: &[&str]) -> ExportedEntity {
    let mut e = entity(id, name);
    e.kind = kind.into();
    e.keys = keys.iter().map(|k| k.to_string()).collect();
    e
}

fn sorted_pair(a: Uuid, b: Uuid) -> Vec<Uuid> {
    let mut ids = vec![a, b];
    ids.sort();
    ids
}

#[test]
fn one_key_placed_on_two_entities_is_a_conflict_under_every_strategy() {
    let e1 = Uuid::new_v4();
    let e2 = Uuid::new_v4();
    let base = empty("test");
    let mut ours = empty("test");
    ours.entities = vec![keyed(e1, "A", "concept", &["k"])];
    let mut theirs = empty("test");
    theirs.entities = vec![keyed(e2, "B", "concept", &["k"])];

    for strategy in [
        SnapshotMergeStrategy::Auto,
        SnapshotMergeStrategy::Ours,
        SnapshotMergeStrategy::Theirs,
    ] {
        let result = three_way_merge(&base, &ours, &theirs, strategy.clone()).unwrap();
        let MergeResult::Conflicts { conflicts } = result else {
            panic!("{strategy:?}: one key on two entities must not merge clean");
        };
        let key_conflicts: Vec<_> = conflicts
            .iter()
            .filter_map(|c| match c {
                MergeConflict::KeyConflict {
                    kind,
                    key,
                    entities,
                } => Some((kind.clone(), key.clone(), entities.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            key_conflicts,
            vec![("concept".to_string(), "k".to_string(), sorted_pair(e1, e2))],
            "{strategy:?}: {conflicts:?}"
        );
    }
}

#[test]
fn one_key_under_two_kinds_names_two_entities_and_merges_clean() {
    let e1 = Uuid::new_v4();
    let e2 = Uuid::new_v4();
    let base = empty("test");
    let mut ours = empty("test");
    ours.entities = vec![keyed(e1, "A", "concept", &["k"])];
    let mut theirs = empty("test");
    theirs.entities = vec![keyed(e2, "B", "person", &["k"])];

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    let MergeResult::Clean { merged } = result else {
        panic!("a key under two kinds is two keys");
    };
    assert_eq!(merged.entities.len(), 2);
}

#[test]
fn a_key_added_on_one_branch_survives_the_merge() {
    let e1 = Uuid::new_v4();
    let mut base = empty("test");
    base.entities = vec![keyed(e1, "A", "concept", &[])];
    let mut ours = empty("test");
    ours.entities = vec![keyed(e1, "A", "concept", &["k"])];
    let theirs = base.clone();

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    let MergeResult::Clean { merged } = result else {
        panic!("a one-sided key addition merges clean");
    };
    assert_eq!(merged.entities[0].keys, vec!["k".to_string()]);
}

#[test]
fn a_key_released_on_one_branch_frees_it_for_a_new_holder() {
    let e1 = Uuid::new_v4();
    let e2 = Uuid::new_v4();
    let mut base = empty("test");
    base.entities = vec![keyed(e1, "A", "concept", &["k"])];
    let mut ours = empty("test");
    ours.entities = vec![
        keyed(e1, "A", "concept", &[]),
        keyed(e2, "B", "concept", &["k"]),
    ];
    let theirs = base.clone();

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    let MergeResult::Clean { merged } = result else {
        panic!("a released key taken by a new entity merges clean");
    };
    let holder = |id: Uuid| {
        merged
            .entities
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.keys.clone())
            .unwrap()
    };
    assert!(holder(e1).is_empty());
    assert_eq!(holder(e2), vec!["k".to_string()]);
}

#[test]
fn a_duplicate_addition_differing_only_in_keys_names_the_keys_field() {
    let id = Uuid::new_v4();
    let base = empty("test");
    let mut ours = empty("test");
    ours.entities = vec![keyed(id, "A", "concept", &["k"])];
    let mut theirs = empty("test");
    theirs.entities = vec![keyed(id, "A", "concept", &["j"])];

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    let MergeResult::Conflicts { conflicts } = result else {
        panic!("one id added twice with different keys must conflict");
    };
    assert!(
        conflicts.iter().any(|c| matches!(
            c,
            MergeConflict::DuplicateAddition { differing_fields, .. }
                if differing_fields == &vec!["keys".to_string()]
        )),
        "{conflicts:?}"
    );
}

fn kind_conflicts(result: MergeResult) -> Vec<(Uuid, String, String)> {
    let MergeResult::Conflicts { conflicts } = result else {
        return Vec::new();
    };
    conflicts
        .into_iter()
        .filter_map(|c| match c {
            MergeConflict::KindConflict {
                entity_id,
                ours,
                theirs,
            } => Some((entity_id, ours, theirs)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_kind_change_that_carries_a_key_to_another_kind_is_a_kind_conflict_under_every_strategy() {
    // Each branch holds k under its own kind, so whichever kind the result
    // takes moves the other branch's key to it.
    let a = Uuid::new_v4();
    let mut base = empty("test");
    base.entities = vec![keyed(a, "A", "concept", &["k"])];
    let mut ours = empty("test");
    ours.entities = vec![keyed(a, "A", "person", &["k"])];
    let theirs = base.clone();

    let expected = vec![(a, "person".to_string(), "concept".to_string())];
    for strategy in [
        SnapshotMergeStrategy::Auto,
        SnapshotMergeStrategy::Ours,
        SnapshotMergeStrategy::Theirs,
    ] {
        let result = three_way_merge(&base, &ours, &theirs, strategy.clone()).unwrap();
        assert_eq!(kind_conflicts(result), expected, "{strategy:?}");
    }
}

#[test]
fn a_kind_change_made_after_every_key_was_released_merges_clean() {
    let a = Uuid::new_v4();
    let mut base = empty("test");
    base.entities = vec![keyed(a, "A", "concept", &["k"])];
    let mut ours = empty("test");
    ours.entities = vec![keyed(a, "A", "person", &[])];
    let theirs = base.clone();

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    let MergeResult::Clean { merged } = result else {
        panic!("a kind change after its key was released merges clean");
    };
    assert_eq!(merged.entities[0].kind, "person");
    assert!(merged.entities[0].keys.is_empty());
}

#[test]
fn a_key_the_other_branch_adds_under_the_old_kind_is_a_kind_conflict() {
    // Both branches change the entity, so the field-level merge names the
    // kind conflict; it is named once.
    let a = Uuid::new_v4();
    let mut base = empty("test");
    base.entities = vec![keyed(a, "A", "concept", &[])];
    let mut ours = empty("test");
    ours.entities = vec![keyed(a, "A", "person", &[])];
    let mut theirs = empty("test");
    theirs.entities = vec![keyed(a, "A", "concept", &["k"])];

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    assert_eq!(
        kind_conflicts(result),
        vec![(a, "person".to_string(), "concept".to_string())]
    );
}

#[test]
fn a_kind_change_to_an_entity_holding_no_keys_merges_clean() {
    let a = Uuid::new_v4();
    let mut base = empty("test");
    base.entities = vec![keyed(a, "A", "concept", &[])];
    let mut ours = empty("test");
    ours.entities = vec![keyed(a, "A", "person", &[])];
    let theirs = base.clone();

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    let MergeResult::Clean { merged } = result else {
        panic!("a kind change without keys merges clean");
    };
    assert_eq!(merged.entities[0].kind, "person");
}

#[test]
fn a_kind_change_on_both_branches_of_a_keyed_entity_is_named_once() {
    let a = Uuid::new_v4();
    let mut base = empty("test");
    base.entities = vec![keyed(a, "A", "concept", &["k"])];
    let mut ours = empty("test");
    ours.entities = vec![keyed(a, "A", "person", &["k"])];
    let mut theirs = empty("test");
    theirs.entities = vec![keyed(a, "A", "project", &["k"])];

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    assert_eq!(
        kind_conflicts(result),
        vec![(a, "person".to_string(), "project".to_string())]
    );
}

#[test]
fn keys_are_compared_under_the_merged_kind() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let mut base = empty("test");
    base.entities = vec![
        keyed(a, "A", "concept", &[]),
        keyed(b, "B", "person", &[]),
    ];
    let mut ours = base.clone();
    ours.entities[0] = keyed(a, "A", "person", &["k"]);
    let mut theirs = base.clone();
    theirs.entities[1] = keyed(b, "B", "person", &["k"]);

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    let MergeResult::Conflicts { conflicts } = result else {
        panic!("two entities of the merged kind holding one key must conflict");
    };
    assert!(
        conflicts.iter().any(|c| matches!(
            c,
            MergeConflict::KeyConflict { kind, key, entities }
                if kind == "person" && key == "k" && entities == &sorted_pair(a, b)
        )),
        "{conflicts:?}"
    );
}

#[test]
fn a_branch_holding_a_key_on_two_entities_is_refused_even_when_the_other_branch_released_it() {
    // One branch releases k from A; the other gives k to B while its own A
    // still holds k. That branch's archive is not a store's state, so the
    // merge refuses it as input instead of reading it as a hand-over.
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let mut base = empty("test");
    base.entities = vec![keyed(a, "A", "concept", &["k"])];
    let mut ours = empty("test");
    ours.entities = vec![keyed(a, "A", "concept", &[])];
    let mut theirs = empty("test");
    theirs.entities = vec![
        keyed(a, "A", "concept", &["k"]),
        keyed(b, "B", "concept", &["k"]),
    ];

    let err = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap_err();
    assert!(
        matches!(&err, MergeError::DuplicateEntityKey { kind, key } if kind == "concept" && key == "k"),
        "{err:?}"
    );
}

#[test]
fn a_key_released_from_one_entity_and_given_to_another_merges_clean() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let mut base = empty("test");
    base.entities = vec![keyed(a, "A", "concept", &["k"])];
    let mut ours = empty("test");
    ours.entities = vec![keyed(a, "A", "concept", &[])];
    let mut theirs = empty("test");
    theirs.entities = vec![
        keyed(a, "A", "concept", &[]),
        keyed(b, "B", "concept", &["k"]),
    ];

    let result = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap();
    let MergeResult::Clean { merged } = result else {
        panic!("k released from A and given to B merges clean");
    };
    let keys_of = |id: Uuid| {
        merged
            .entities
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.keys.clone())
            .unwrap()
    };
    assert!(keys_of(a).is_empty());
    assert_eq!(keys_of(b), vec!["k".to_string()]);
}

#[test]
fn an_input_archive_with_one_key_on_two_entities_is_refused_before_any_diff() {
    let e1 = Uuid::new_v4();
    let e2 = Uuid::new_v4();
    let base = empty("test");
    let mut ours = empty("test");
    ours.entities = vec![
        keyed(e1, "A", "concept", &["k"]),
        keyed(e2, "B", "concept", &["k"]),
    ];
    let theirs = empty("test");

    let err = three_way_merge(&base, &ours, &theirs, SnapshotMergeStrategy::Auto).unwrap_err();
    assert!(
        matches!(&err, MergeError::DuplicateEntityKey { kind, key } if kind == "concept" && key == "k"),
        "{err:?}"
    );
}
