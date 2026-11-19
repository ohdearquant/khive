// ── Extensibility ────────────────────────────────────────────────────────

#[test]
fn register_brain_profile_for_concept() {
    let mut r = EntityTypeRegistry::builtin();
    r.register(EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "brain_profile",
        aliases: &[],
    });
    let res = r
        .resolve(EntityKind::Concept, Some("brain_profile"))
        .expect("brain_profile registered for Concept");
    assert_eq!(res.entity_type.as_deref(), Some("brain_profile"));
}

#[test]
fn with_extra_adds_subtypes() {
    // Use a pack-specific subtype that is NOT in BUILTIN_DEFS.
    let r = EntityTypeRegistry::with_extra([EntityTypeDef {
        kind: EntityKind::Service,
        type_name: "grpc_service",
        aliases: &["grpc"],
    }]);
    let res = r
        .resolve(EntityKind::Service, Some("grpc"))
        .expect("grpc alias for grpc_service");
    assert_eq!(res.entity_type.as_deref(), Some("grpc_service"));
}

#[test]
fn service_api_is_builtin() {
    let r = reg();
    let res = r
        .resolve(EntityKind::Service, Some("api"))
        .expect("api is a built-in Service subtype");
    assert_eq!(res.entity_type.as_deref(), Some("api"));
    let res2 = r
        .resolve(EntityKind::Service, Some("endpoint"))
        .expect("endpoint is an alias for api");
    assert_eq!(res2.entity_type.as_deref(), Some("api"));
}

#[test]
fn service_mcp_server_is_builtin() {
    let r = reg();
    let res = r
        .resolve(EntityKind::Service, Some("mcp_server"))
        .expect("mcp_server is a built-in Service subtype");
    assert_eq!(res.entity_type.as_deref(), Some("mcp_server"));
    let res2 = r
        .resolve(EntityKind::Service, Some("mcp"))
        .expect("mcp is an alias for mcp_server");
    assert_eq!(res2.entity_type.as_deref(), Some("mcp_server"));
}

#[test]
fn project_has_no_service_subtype() {
    let r = reg();
    r.resolve(EntityKind::Project, Some("service"))
        .expect_err("service must not be a Project subtype");
    r.resolve(EntityKind::Project, Some("svc"))
        .expect_err("svc must not be a Project subtype");
}

#[test]
fn benchmark_is_dataset_not_concept() {
    let r = reg();
    let res = r
        .resolve(EntityKind::Dataset, Some("benchmark"))
        .expect("benchmark is a valid Dataset subtype");
    assert_eq!(res.entity_type.as_deref(), Some("benchmark"));
    r.resolve(EntityKind::Concept, Some("benchmark"))
        .expect_err("benchmark must not be a Concept subtype");
}

#[test]
fn model_alias_resolves_to_model_family() {
    let r = reg();
    let res = r
        .resolve(EntityKind::Concept, Some("model"))
        .expect("model is an alias for model_family");
    assert_eq!(res.entity_type.as_deref(), Some("model_family"));
    let res2 = r
        .resolve(EntityKind::Concept, Some("model_family"))
        .expect("model_family is the canonical name");
    assert_eq!(res2.entity_type.as_deref(), Some("model_family"));
}

// ── Case insensitivity ───────────────────────────────────────────────────

#[test]
fn resolve_is_case_insensitive() {
    let r = reg();
    let res = r
        .resolve(EntityKind::Concept, Some("Algorithm"))
        .expect("Algorithm (mixed case) must resolve");
    assert_eq!(res.entity_type.as_deref(), Some("algorithm"));
}

// ── valid_types_for ──────────────────────────────────────────────────────

#[test]
fn valid_types_for_person_is_none_registered() {
    let r = reg();
    let s = r.valid_types_for(EntityKind::Person);
    assert_eq!(
        s, "(none registered)",
        "Person has no built-in subtypes; got: {s}"
    );
}

#[test]
fn valid_types_for_concept_includes_algorithm() {
    let r = reg();
    let s = r.valid_types_for(EntityKind::Concept);
    assert!(
        s.contains("algorithm"),
        "Concept valid types must include algorithm; got: {s}"
    );
}

#[test]
fn resolve_inductive_alias_to_structure() {
    let r = reg();
    let res = r
        .resolve(EntityKind::Concept, Some("inductive"))
        .expect("inductive is a valid alias for structure");
    assert_eq!(res.kind, EntityKind::Concept);
    assert_eq!(res.entity_type.as_deref(), Some("structure"));
}

#[test]
fn resolve_goal_subtype() {
    let r = reg();
    let res = r
        .resolve(EntityKind::Concept, Some("goal"))
        .expect("goal is a valid Concept subtype");
    assert_eq!(res.kind, EntityKind::Concept);
    assert_eq!(res.entity_type.as_deref(), Some("goal"));
}

// ── Global registry ──────────────────────────────────────────────────────

#[cfg(feature = "std")]
#[test]
fn global_registry_is_accessible() {
    let r = EntityTypeRegistry::global();
    let res = r
        .resolve(EntityKind::Document, Some("paper"))
        .expect("global registry must resolve paper");
    assert_eq!(res.entity_type.as_deref(), Some("paper"));
}

// ── snake_case normalisation (ADR-001:106) ───────────────────────────────

#[test]
fn to_snake_case_converts_hyphen_to_underscore() {
    assert_eq!(to_snake_case("proof-goal"), "proof_goal");
}

#[test]
fn to_snake_case_converts_space_to_underscore() {
    assert_eq!(to_snake_case("Proof Goal"), "proof_goal");
}

#[test]
fn to_snake_case_collapses_mixed_separators() {
    assert_eq!(to_snake_case("proof - goal"), "proof_goal");
    assert_eq!(to_snake_case("proof__goal"), "proof_goal");
}

#[test]
fn to_snake_case_strips_leading_trailing_separators() {
    assert_eq!(to_snake_case("-theorem-"), "theorem");
    assert_eq!(to_snake_case(" theorem "), "theorem");
}

#[test]
fn resolve_proof_goal_hyphen_normalises_to_goal() {
    let r = reg();
    let res = r
        .resolve(EntityKind::Concept, Some("proof-goal"))
        .expect("proof-goal must resolve via snake_case normalisation + alias");
    assert_eq!(res.entity_type.as_deref(), Some("goal"));
}

#[test]
fn resolve_proof_goal_space_normalises_to_goal() {
    let r = reg();
    let res = r
        .resolve(EntityKind::Concept, Some("Proof Goal"))
        .expect("Proof Goal must resolve via snake_case normalisation + alias");
    assert_eq!(res.entity_type.as_deref(), Some("goal"));
}

#[test]
fn resolve_blog_hyphen_normalises_to_blog_post_non_formal() {
    let r = reg();
    let res = r
        .resolve(EntityKind::Document, Some("blog-post"))
        .expect("blog-post must normalise to blog_post");
    assert_eq!(res.entity_type.as_deref(), Some("blog_post"));
}
