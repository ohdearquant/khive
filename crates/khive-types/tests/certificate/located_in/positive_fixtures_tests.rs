// ── Positive tests ───────────────────────────────────────────────────────────

/// The positive fixtures, one per eliminator family plus the second Er fixture for the
/// `extends` signature collision, exported for the closed-set coverage gate.
pub const FIXTURES: &[Fixture] = &[CV, ER, ER_EXTENDS, AT, PO, CH, MV, SR];

#[test]
fn cv_located_in_is_not_converse_of_contains() {
    assert_defeats_eliminator(R, &CV);
}

#[test]
fn er_located_in_is_not_part_of_restricted_to_disorder_anatomy() {
    assert_defeats_eliminator(R, &ER);
}

#[test]
fn er_located_in_is_not_extends_over_the_shared_concept_signature() {
    assert_defeats_eliminator(R, &ER_EXTENDS);
}

#[test]
fn at_located_in_is_not_part_of_with_a_location_role() {
    assert_defeats_eliminator(R, &AT);
}

#[test]
fn po_located_in_has_no_polarity_partner() {
    assert_defeats_eliminator(R, &PO);
}

#[test]
fn ch_located_in_is_not_a_chain_of_existing_relations() {
    assert_defeats_eliminator(R, &CH);
}

#[test]
fn mv_located_in_is_not_a_reachability_view() {
    assert_defeats_eliminator(R, &MV);
}

#[test]
fn sr_located_in_is_not_a_sub_relation_of_links_to() {
    assert_defeats_eliminator(R, &SR);
}

/// The ER fixture actually exercises the anatomy -> anatomy pair the ADR names: the
/// restriction is empty, so encoding `lung located_in thoracic_cavity` as part_of would
/// have to add a constituent-of-a-space claim the restriction cannot express.
#[test]
fn er_fixture_contains_the_anatomy_to_anatomy_pair() {
    assert!(ER
        .graph
        .contains(&("lung", "located_in", "thoracic_cavity")));
    assert_eq!(kind_of(ER.graph, "lung"), Some("anatomy"));
    assert_eq!(kind_of(ER.graph, "thoracic_cavity"), Some("anatomy"));
}

/// The `extends` fixture carries a real lineage pair beside both location pairs, every
/// endpoint on the shared `concept -> concept` signature, so the restricted relation is
/// non-empty and still differs from `located_in`.
#[test]
fn er_extends_fixture_pairs_lineage_with_location_on_the_shared_signature() {
    assert!(ER_EXTENDS
        .graph
        .contains(&("bacterial_pneumonia", "extends", "pneumonia")));
    assert!(ER_EXTENDS
        .graph
        .contains(&("pneumonia", "located_in", "lung")));
    assert!(ER_EXTENDS
        .graph
        .contains(&("lung", "located_in", "thoracic_cavity")));
    for node in [
        "lung",
        "thoracic_cavity",
        "pneumonia",
        "bacterial_pneumonia",
    ] {
        assert_eq!(kind_of(ER_EXTENDS.graph, node), Some("concept"));
    }
}

#[test]
fn full_certificate_located_in_defeats_all_seven_eliminators() {
    run_certificate(R, FIXTURES);
}
