use khive_types::{EntityKind, EntityTypeDef, EntityTypeRegistry};

#[test]
fn same_owner_alias_and_canonical_collision_is_rejected_in_either_order() {
    const ALIASES: [&[&str]; 3] = [&["widget_target"], &["Widget-Target"], &[" widget target "]];
    for aliases in ALIASES {
        let source = EntityTypeDef {
            kind: EntityKind::Service,
            type_name: "widget_source",
            aliases,
        };
        let target = EntityTypeDef {
            kind: EntityKind::Service,
            type_name: "widget_target",
            aliases: &[],
        };
        for defs in [[&source, &target], [&target, &source]] {
            let error =
                EntityTypeRegistry::check_extra_collisions(defs.map(|def| ("widget_pack", def)))
                    .expect_err("an alias cannot become another subtype's canonical name");
            assert!(error.contains("service:widget_target"), "{error}");
            assert!(error.contains("widget_pack"), "{error}");
            assert!(error.contains("canonical \"widget_source\""), "{error}");
            assert!(error.contains("canonical \"widget_target\""), "{error}");
        }
    }
}

#[test]
fn same_owner_aliases_cannot_select_different_canonical_types() {
    let source = EntityTypeDef {
        kind: EntityKind::Service,
        type_name: "widget_source",
        aliases: &["shared_widget"],
    };
    let target = EntityTypeDef {
        kind: EntityKind::Service,
        type_name: "widget_target",
        aliases: &["Shared-Widget"],
    };
    for defs in [[&source, &target], [&target, &source]] {
        assert!(
            EntityTypeRegistry::check_extra_collisions(defs.map(|def| ("widget_pack", def)))
                .is_err()
        );
    }
}

#[test]
fn same_owner_repeated_canonical_and_self_alias_remain_valid() {
    let def = EntityTypeDef {
        kind: EntityKind::Service,
        type_name: "widget_service",
        aliases: &["Widget-Service", "widget_alias"],
    };
    EntityTypeRegistry::check_extra_collisions([("widget_pack", &def), ("widget_pack", &def)])
        .unwrap();
    let registry = EntityTypeRegistry::with_extra([def.clone(), def]);
    for spelling in ["widget_service", "Widget-Service", "widget_alias"] {
        assert_eq!(
            registry
                .resolve(EntityKind::Service, Some(spelling))
                .unwrap()
                .entity_type
                .as_deref(),
            Some("widget_service")
        );
    }
}

#[test]
fn alias_keys_remain_scoped_to_the_entity_kind() {
    let service = EntityTypeDef {
        kind: EntityKind::Service,
        type_name: "widget_service",
        aliases: &["widget_record"],
    };
    let document = EntityTypeDef {
        kind: EntityKind::Document,
        type_name: "widget_record",
        aliases: &[],
    };
    for defs in [[&service, &document], [&document, &service]] {
        EntityTypeRegistry::check_extra_collisions(defs.map(|def| ("widget_pack", def))).unwrap();
        let registry = EntityTypeRegistry::with_extra(defs.into_iter().cloned());
        for (kind, expected) in [
            (EntityKind::Service, "widget_service"),
            (EntityKind::Document, "widget_record"),
        ] {
            assert_eq!(
                registry
                    .resolve(kind, Some("widget_record"))
                    .unwrap()
                    .entity_type
                    .as_deref(),
                Some(expected)
            );
        }
    }
}

#[test]
fn different_owners_cannot_repeat_a_canonical_type() {
    let def = EntityTypeDef {
        kind: EntityKind::Service,
        type_name: "widget_service",
        aliases: &[],
    };
    let error =
        EntityTypeRegistry::check_extra_collisions([("first_pack", &def), ("second_pack", &def)])
            .unwrap_err();
    assert_eq!(error, "duplicate entity_type \"service:widget_service\": claimed by both \"first_pack\" and \"second_pack\"");
}
