use super::*;
use crate::entity::EntityKind;

fn reg() -> EntityTypeRegistry {
    EntityTypeRegistry::builtin()
}

include!("entity_type_tests/resolution_and_rejection_tests.rs");

include!("entity_type_tests/registry_extension_and_normalization_tests.rs");

// ── ADR-085 code subtypes ────────────────────────────────────────────────

#[test]
fn entity_type_registry_accepts_code_tokens_and_aliases() {
    let r = reg();
    for (raw, canonical) in [
        ("module", "module"),
        ("mod", "module"),
        ("namespace", "module"),
        ("function", "function"),
        ("fn", "function"),
        ("func", "function"),
        ("method", "function"),
        ("datatype", "datatype"),
        ("enum", "datatype"),
        ("record", "datatype"),
        ("type_alias", "datatype"),
        ("interface", "interface"),
        ("trait", "interface"),
        ("protocol", "interface"),
    ] {
        let res = r
            .resolve(EntityKind::Concept, Some(raw))
            .unwrap_or_else(|e| panic!("{raw:?} must resolve for Concept: {e}"));
        assert_eq!(
            res.entity_type.as_deref(),
            Some(canonical),
            "{raw:?} must resolve to {canonical:?}"
        );
    }
}

#[test]
fn entity_type_registry_does_not_claim_struct_or_class_for_code() {
    let r = reg();
    let struct_res = r
        .resolve(EntityKind::Concept, Some("struct"))
        .expect("struct remains a valid Concept subtype (owned by formal structure)");
    assert_eq!(
        struct_res.entity_type.as_deref(),
        Some("structure"),
        "struct must still resolve to formal-math structure, not datatype"
    );
    let class_res = r
        .resolve(EntityKind::Concept, Some("class"))
        .expect("class remains a valid Concept subtype (owned by formal structure)");
    assert_eq!(
        class_res.entity_type.as_deref(),
        Some("structure"),
        "class must still resolve to formal-math structure, not datatype"
    );
}

// ADR-191: the web pack's registry footprint shrank from five ARW-shaped
// tokens to three web-native ones. This test is the acceptance witness
// for that deletion — it accepts the surviving tokens under their kinds
// and refuses the deleted ones under every kind, not just their former one.
#[test]
fn entity_type_registry_accepts_web_tokens_and_aliases() {
    let r = reg();
    for (kind, canonical, alias) in [
        (EntityKind::Service, "site", "origin"),
        (EntityKind::Document, "page", "web_page"),
    ] {
        for raw in [canonical, alias] {
            let resolved = r
                .resolve(kind, Some(raw))
                .unwrap_or_else(|e| panic!("{raw:?} must resolve for {kind}: {e}"));
            assert_eq!(resolved.kind, kind);
            assert_eq!(resolved.entity_type.as_deref(), Some(canonical));
            for other_kind in EntityKind::ALL {
                if other_kind != kind {
                    r.resolve(other_kind, Some(raw))
                        .expect_err("web subtypes belong to exactly one base kind");
                }
            }
        }
    }

    // `resource` has no alias.
    let resolved = r
        .resolve(EntityKind::Document, Some("resource"))
        .expect("resource must resolve for Document");
    assert_eq!(resolved.kind, EntityKind::Document);
    assert_eq!(resolved.entity_type.as_deref(), Some("resource"));
    for other_kind in EntityKind::ALL {
        if other_kind != EntityKind::Document {
            r.resolve(other_kind, Some("resource"))
                .expect_err("resource belongs to exactly one base kind");
        }
    }

    // Deleted ARW-shaped tokens (and their aliases) must be refused under
    // every kind, not just their former one — a partial deletion that
    // left one arm registered would otherwise pass silently.
    for gone in [
        "machine_view",
        "view",
        "agent_tool",
        "mcp_tool",
        "agent_skill",
        "skill_manifest",
    ] {
        for kind in EntityKind::ALL {
            r.resolve(kind, Some(gone)).expect_err(
                "deleted web token must be refused for every kind, not just its former one",
            );
        }
    }
}

#[test]
fn web_tokens_and_aliases_have_unique_registry_owners() {
    for token in ["site", "origin", "page", "web_page", "resource"] {
        let owners: Vec<_> = BUILTIN_DEFS
            .iter()
            .filter(|def| {
                to_snake_case(def.type_name) == token
                    || def
                        .aliases
                        .iter()
                        .any(|alias| to_snake_case(alias) == token)
            })
            .collect();
        assert_eq!(
            owners.len(),
            1,
            "{token:?} has conflicting owners: {owners:?}"
        );
    }
}

#[test]
fn web_vocabulary_preserves_tool_as_a_project_subtype() {
    let r = reg();
    assert_eq!(
        r.resolve(EntityKind::Project, Some("tool"))
            .expect("tool remains a project subtype")
            .entity_type
            .as_deref(),
        Some("tool")
    );
    r.resolve(EntityKind::Service, Some("tool"))
        .expect_err("tool is not a service subtype");
    r.resolve(EntityKind::Document, Some("skill"))
        .expect_err("skill is not a document subtype");
}

// ── check_extra_collisions: ADR-001 normalisation (PR #925) ──
//
// `check_extra_collisions` must reject collisions that only become
// visible after ADR-001:104-107's write-time `to_snake_case`
// normalisation — i.e. two raw spellings that differ in case or
// separator style but land on the same kind-qualified key once
// normalised. Before the r2 fix these three cases silently passed
// (`Ok(())`) because the check compared raw, unnormalised strings.

#[test]
fn check_extra_collisions_detects_normalized_alias_vs_alias() {
    let def_a = EntityTypeDef {
        kind: EntityKind::Service,
        type_name: "widget_service",
        aliases: &["Widget-Alias"],
    };
    let def_b = EntityTypeDef {
        kind: EntityKind::Service,
        type_name: "gadget_service",
        aliases: &["widget_alias"],
    };
    let err = EntityTypeRegistry::check_extra_collisions([("pack_a", &def_a), ("pack_b", &def_b)])
        .expect_err(
            "aliases that only differ in case/hyphenation but normalise to the same \
             kind-qualified key must collide",
        );
    assert!(err.contains("pack_a"), "error must name pack_a: {err}");
    assert!(err.contains("pack_b"), "error must name pack_b: {err}");
}

#[test]
fn check_extra_collisions_detects_normalized_alias_vs_canonical() {
    let def_a = EntityTypeDef {
        kind: EntityKind::Service,
        type_name: "primary_service",
        aliases: &[],
    };
    let def_b = EntityTypeDef {
        kind: EntityKind::Service,
        type_name: "secondary_service",
        aliases: &["Primary-Service"],
    };
    let err = EntityTypeRegistry::check_extra_collisions([("pack_a", &def_a), ("pack_b", &def_b)])
        .expect_err(
            "an alias that normalises onto another owner's canonical key must collide, \
             even when the raw spellings differ",
        );
    assert!(err.contains("pack_a"), "error must name pack_a: {err}");
    assert!(err.contains("pack_b"), "error must name pack_b: {err}");
}

#[test]
fn check_extra_collisions_detects_normalized_builtin_vs_pack_collision() {
    // BUILTIN_DEFS declares Document "paper". A pack declaring a
    // differently-cased spelling that normalises onto the same
    // kind-qualified key must still collide with the built-in owner.
    let def = EntityTypeDef {
        kind: EntityKind::Document,
        type_name: "Paper",
        aliases: &[],
    };
    let err = EntityTypeRegistry::check_extra_collisions([("pack_a", &def)]).expect_err(
        "a pack canonical name that normalises onto a builtin canonical key must collide",
    );
    assert!(err.contains("builtin"), "error must name builtin: {err}");
    assert!(err.contains("pack_a"), "error must name pack_a: {err}");
}
