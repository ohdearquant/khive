// ── Negative controls: graphs in which `located_in` really is redundant ──────

#[test]
fn cv_control_located_in_as_converse_of_contains_is_eliminated() {
    const G: Fixture = Fixture {
        graph: &[
            ("lung", "contains", "right_lung"),
            ("right_lung", "located_in", "lung"),
        ],
        ..CV
    };
    assert_eliminated_by(R, &G);
}

#[test]
fn er_control_located_in_as_restricted_part_of_is_eliminated() {
    const G: Fixture = Fixture {
        graph: &[
            ("lung", "kind", "anatomy"),
            ("pneumonia", "kind", "disorder"),
            ("pneumonia", "part_of", "lung"),
            ("pneumonia", "located_in", "lung"),
        ],
        ..ER
    };
    assert_eliminated_by(R, &G);
}

#[test]
fn er_control_located_in_as_concept_extends_is_eliminated() {
    const G: Fixture = Fixture {
        graph: &[
            ("a", "kind", "concept"),
            ("b", "kind", "concept"),
            ("a", "extends", "b"),
            ("a", "located_in", "b"),
        ],
        ..ER_EXTENDS
    };
    assert_eliminated_by(R, &G);
}

#[test]
fn at_control_located_in_as_attribute_blind_part_of_is_eliminated() {
    const G: Fixture = Fixture {
        graph: &[
            ("pneumonia", "part_of", "lung"),
            ("pneumonia", "attr:role:location", "lung"),
            ("pneumonia", "located_in", "lung"),
        ],
        ..AT
    };
    assert_eliminated_by(R, &G);
}

#[test]
fn po_control_located_in_as_half_of_a_partition_is_eliminated() {
    const G: Fixture = Fixture {
        graph: &[
            ("x", "located_in", "y"),
            ("x", "contains", "z"),
            ("x", "part_of", "y"),
            ("x", "part_of", "z"),
        ],
        ..PO
    };
    assert_eliminated_by(R, &G);
}

#[test]
fn ch_control_located_in_as_a_two_step_chain_is_eliminated() {
    const G: Fixture = Fixture {
        graph: &[
            ("a", "part_of", "b"),
            ("b", "part_of", "c"),
            ("a", "located_in", "c"),
        ],
        ..CH
    };
    assert_eliminated_by(R, &G);
}

#[test]
fn mv_control_located_in_as_a_reachability_view_is_eliminated() {
    const G: Fixture = Fixture {
        graph: &[
            ("a", "part_of", "b"),
            ("b", "part_of", "c"),
            ("a", "located_in", "b"),
            ("a", "located_in", "c"),
        ],
        ..MV
    };
    assert_eliminated_by(R, &G);
}

#[test]
fn sr_control_located_in_as_a_subset_of_links_to_is_eliminated() {
    const G: Fixture = Fixture {
        graph: &[("a", "links_to", "b"), ("a", "located_in", "b")],
        ..SR
    };
    assert_eliminated_by(R, &G);
}
