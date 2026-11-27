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
    let (changed, statements, report) = apply_entity_alias_change(&mut entity, &change, 7);
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
    let (changed, statements, report) = apply_entity_alias_change(&mut entity, &change, 7);
    assert!(changed);
    assert_eq!(statements.len(), 1);
    assert_eq!(report.not_removed, aliases(&["absent", "merged"]));
    assert_eq!(
        held(&entity),
        vec![("merged".to_owned(), EntityAliasOrigin::Merge)]
    );
}

#[test]
fn adding_a_held_alias_changes_nothing() {
    let mut entity = Entity::new("ns", "concept", "Name");
    entity.aliases = vec![EntityAlias {
        alias: "former".into(),
        origin: EntityAliasOrigin::Rename,
    }];
    let change = EntityAliasChange::update(&aliases(&["former"]), &[]).unwrap();
    let (changed, statements, report) = apply_entity_alias_change(&mut entity, &change, 7);
    assert!(!changed);
    assert!(statements.is_empty());
    assert!(report.is_empty());
}
