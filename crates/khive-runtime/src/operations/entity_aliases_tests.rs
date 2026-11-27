use super::*;

fn aliases(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn held(entity: &Entity) -> Vec<(String, EntityAliasOrigin)> {
    entity
        .aliases
        .iter()
        .map(|held| (held.alias.clone(), held.origin))
        .collect()
}

#[test]
fn alias_validation_refuses_empty_long_and_nul() {
    for bad in [String::new(), "a".repeat(513), "a\0b".to_owned()] {
        let error = validate_entity_alias(&bad).unwrap_err();
        assert!(
            error.to_string().contains(ENTITY_ALIAS_REFUSAL),
            "{bad:?}: {error}"
        );
    }
    validate_entity_alias(&"a".repeat(512)).unwrap();
}

#[test]
fn an_alias_both_added_and_removed_is_refused() {
    let error = EntityAliasChange::update(&aliases(&["x"]), &aliases(&["x"])).unwrap_err();
    assert!(
        error.to_string().contains("both added and removed"),
        "{error}"
    );
}

#[test]
fn replace_keeps_other_origins_and_drops_the_name() {
    let mut entity = Entity::new("ns", "concept", "Name");
    entity.aliases = vec![
        EntityAlias {
            alias: "kept".into(),
            origin: EntityAliasOrigin::Caller,
        },
        EntityAlias {
            alias: "old".into(),
            origin: EntityAliasOrigin::Caller,
        },
        EntityAlias {
            alias: "former".into(),
            origin: EntityAliasOrigin::Rename,
        },
    ];
    let change = EntityAliasChange::replace(&aliases(&["new", "kept", "Name"])).unwrap();
    let (changed, statements, report) =
        apply_entity_alias_change(&mut entity, &change, 7, Some("Name"));
    assert!(changed);
    assert_eq!(report.not_stored, aliases(&["Name"]));
    assert_eq!(
        held(&entity),
        vec![
            ("former".to_owned(), EntityAliasOrigin::Rename),
            ("kept".to_owned(), EntityAliasOrigin::Caller),
            ("new".to_owned(), EntityAliasOrigin::Caller),
        ]
    );
    let labels: Vec<&str> = statements
        .iter()
        .map(|statement| statement.label.as_deref().unwrap())
        .collect();
    assert_eq!(
        labels,
        ["entity-caller-alias-delete", "entity-alias-insert"]
    );
}

#[test]
fn update_lists_removals_it_cannot_make() {
    let mut entity = Entity::new("ns", "concept", "Name");
    entity.aliases = vec![
        EntityAlias {
            alias: "mine".into(),
            origin: EntityAliasOrigin::Caller,
        },
        EntityAlias {
            alias: "merged".into(),
            origin: EntityAliasOrigin::Merge,
        },
    ];
    let change = EntityAliasChange::update(&[], &aliases(&["mine", "merged", "absent"])).unwrap();
    let (changed, statements, report) =
        apply_entity_alias_change(&mut entity, &change, 7, Some("Name"));
    assert!(changed);
    assert_eq!(statements.len(), 1);
    assert_eq!(report.not_removed, aliases(&["absent", "merged"]));
    assert_eq!(
        held(&entity),
        vec![("merged".to_owned(), EntityAliasOrigin::Merge)]
    );
}

#[test]
fn a_rename_onto_a_held_alias_removes_it_whatever_wrote_it() {
    for origin in [
        EntityAliasOrigin::Caller,
        EntityAliasOrigin::Rename,
        EntityAliasOrigin::Merge,
    ] {
        let mut entity = Entity::new("ns", "concept", "old");
        entity.aliases = vec![
            EntityAlias {
                alias: "kept".into(),
                origin: EntityAliasOrigin::Merge,
            },
            EntityAlias {
                alias: "new".into(),
                origin,
            },
        ];
        entity.name = "new".into();
        let (changed, statements, report) = apply_entity_alias_change(
            &mut entity,
            &EntityAliasChange::default().keeping_old_name(false),
            7,
            Some("old"),
        );
        assert!(changed, "{origin:?}");
        assert!(report.is_empty(), "{origin:?}");
        assert_eq!(
            held(&entity),
            vec![("kept".to_owned(), EntityAliasOrigin::Merge)],
            "{origin:?}"
        );
        let labels: Vec<&str> = statements
            .iter()
            .map(|statement| statement.label.as_deref().unwrap())
            .collect();
        assert_eq!(labels, ["entity-name-alias-delete"], "{origin:?}");
    }
}

fn labels(statements: &[SqlStatement]) -> Vec<&str> {
    statements
        .iter()
        .map(|statement| statement.label.as_deref().unwrap())
        .collect()
}

#[test]
fn a_rename_keeps_the_old_name_as_a_rename_alias() {
    let mut entity = Entity::new("ns", "concept", "New");
    entity.aliases = vec![EntityAlias {
        alias: "New".into(),
        origin: EntityAliasOrigin::Caller,
    }];
    let (changed, statements, report) =
        apply_entity_alias_change(&mut entity, &EntityAliasChange::default(), 7, Some("Old"));
    assert!(changed);
    assert!(report.is_empty());
    assert_eq!(
        held(&entity),
        vec![("Old".to_owned(), EntityAliasOrigin::Rename)]
    );
    assert_eq!(
        labels(&statements),
        ["entity-name-alias-delete", "entity-alias-insert"]
    );
}

#[test]
fn keep_old_name_false_keeps_no_alias() {
    let mut entity = Entity::new("ns", "concept", "New");
    let change = EntityAliasChange::default().keeping_old_name(false);
    let (changed, statements, _) = apply_entity_alias_change(&mut entity, &change, 7, Some("Old"));
    assert!(!changed);
    assert!(statements.is_empty());
    assert!(entity.aliases.is_empty());
}

#[test]
fn the_old_name_named_as_a_caller_alias_by_the_same_change_stays_caller() {
    let mut entity = Entity::new("ns", "concept", "New");
    let change = EntityAliasChange::update(&aliases(&["Old"]), &[]).unwrap();
    let (changed, statements, _) = apply_entity_alias_change(&mut entity, &change, 7, Some("Old"));
    assert!(changed);
    assert_eq!(
        held(&entity),
        vec![("Old".to_owned(), EntityAliasOrigin::Caller)]
    );
    assert_eq!(labels(&statements), ["entity-alias-insert"]);
}

#[test]
fn an_unchanged_name_keeps_no_alias() {
    let mut entity = Entity::new("ns", "concept", "Same");
    let (changed, statements, _) =
        apply_entity_alias_change(&mut entity, &EntityAliasChange::default(), 7, Some("Same"));
    assert!(!changed);
    assert!(statements.is_empty());
}

#[test]
fn adding_a_held_alias_changes_nothing() {
    let mut entity = Entity::new("ns", "concept", "Name");
    entity.aliases = vec![EntityAlias {
        alias: "former".into(),
        origin: EntityAliasOrigin::Rename,
    }];
    let change = EntityAliasChange::update(&aliases(&["former"]), &[]).unwrap();
    let (changed, statements, report) =
        apply_entity_alias_change(&mut entity, &change, 7, Some("Name"));
    assert!(!changed);
    assert!(statements.is_empty());
    assert!(report.is_empty());
}
