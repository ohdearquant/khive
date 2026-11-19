// ── Basic happy-path resolution ──────────────────────────────────────────

#[test]
fn research_report_resolves_in_every_stored_spelling() {
    let r = reg();
    for raw in ["research-report", "research_report", "Research Report"] {
        let res = r
            .resolve(EntityKind::Document, Some(raw))
            .unwrap_or_else(|e| panic!("{raw} must resolve as a Document subtype: {e}"));
        assert_eq!(res.entity_type.as_deref(), Some("research_report"), "{raw}");
    }
    // A Document genre, not a Concept one: the same spelling on another
    // kind is still refused, so the registry stays kind-scoped.
    assert!(r
        .resolve(EntityKind::Concept, Some("research-report"))
        .is_err());
}

#[test]
fn resolve_paper_infers_document() {
    let r = reg();
    let res = r
        .resolve(EntityKind::Document, Some("paper"))
        .expect("paper is a valid Document subtype");
    assert_eq!(res.kind, EntityKind::Document);
    assert_eq!(res.entity_type.as_deref(), Some("paper"));
}

#[test]
fn resolve_none_entity_type_always_ok() {
    let r = reg();
    for kind in EntityKind::ALL {
        let res = r.resolve(kind, None).expect("None entity_type always ok");
        assert_eq!(res.entity_type, None);
    }
}

#[test]
fn resolve_algo_alias_to_algorithm() {
    let r = reg();
    let res = r
        .resolve(EntityKind::Concept, Some("algo"))
        .expect("algo is a valid alias for algorithm");
    assert_eq!(res.kind, EntityKind::Concept);
    assert_eq!(res.entity_type.as_deref(), Some("algorithm"));
}

#[test]
fn resolve_spec_alias_to_specification() {
    let r = reg();
    let res = r
        .resolve(EntityKind::Document, Some("spec"))
        .expect("spec is alias for specification");
    assert_eq!(res.entity_type.as_deref(), Some("specification"));
}

// ── Rejection tests ──────────────────────────────────────────────────────

#[test]
fn reject_brain_profile_for_concept() {
    let r = reg();
    let err = r
        .resolve(EntityKind::Concept, Some("brain_profile"))
        .expect_err("brain_profile is not a Concept subtype");
    let msg = format!("{err}");
    assert!(
        msg.contains("brain_profile"),
        "error must mention the rejected type; got: {msg}"
    );
    assert!(
        msg.contains("concept"),
        "error must mention the target kind; got: {msg}"
    );
}

#[test]
fn reject_unknown_subtype_with_valid_list() {
    let r = reg();
    let err = r
        .resolve(EntityKind::Document, Some("mystery_type"))
        .expect_err("mystery_type must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("mystery_type"),
        "error must echo the rejected value; got: {msg}"
    );
    // Valid subtypes for Document must appear.
    assert!(
        msg.contains("paper"),
        "error must list valid Document subtypes; got: {msg}"
    );
}

#[test]
fn reject_wrong_kind_subtype_mentions_correct_kind() {
    // "paper" belongs to Document; passing it for Concept must fail.
    let r = reg();
    let err = r
        .resolve(EntityKind::Concept, Some("paper"))
        .expect_err("paper is a Document subtype, not Concept");
    let msg = format!("{err}");
    assert!(
        msg.contains("document") || msg.contains("Document"),
        "error must name the correct kind; got: {msg}"
    );
}
