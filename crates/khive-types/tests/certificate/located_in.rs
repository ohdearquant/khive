//! ADR-076 non-redundancy certificate fixtures for the `located_in` relation (ADR-196).
//!
//! `located_in` says the source occupies, or is manifested in, the target and is not a
//! constituent of it: a disorder in an organ, an organ in a body cavity. Wire direction is
//! located -> location.
//!
//! The fixture is the one ADR-196 names: `lung`, `respiratory system` and `right lung`
//! (anatomy), `pneumonia` (disorder), with `right lung part_of lung`,
//! `lung part_of respiratory system` and `pneumonia located_in lung`; the Er fixture adds
//! `thoracic cavity` and `lung located_in thoracic cavity`.
//!
//! Every check derives both answers from the fixture graph. Each check is generic over the
//! relations present in the graph, so the negative controls at the bottom of the file feed
//! the same check functions a graph in which `located_in` really is redundant and require
//! them to fire: a check that could never return `Eliminated` would certify nothing.

use std::collections::BTreeSet;

use crate::harness::{
    all_pairs, assert_defeats_eliminator, assert_eliminated_by, fmt_pairs, reachable,
    run_certificate, EliminatorCheck, Fixture, GraphTriple,
};

const R: &str = "located_in";

type Pairs = BTreeSet<(String, String)>;

/// Names of the existing relations in a graph: everything except the candidate, the
/// `kind` annotation and `attr:*` markers.
fn existing_relations(graph: &[GraphTriple]) -> BTreeSet<&'static str> {
    graph
        .iter()
        .map(|(_, rel, _)| *rel)
        .filter(|rel| *rel != R && *rel != "kind" && !rel.starts_with("attr:"))
        .collect()
}

fn kind_of(graph: &[GraphTriple], node: &str) -> Option<&'static str> {
    graph
        .iter()
        .find(|(n, rel, _)| *n == node && *rel == "kind")
        .map(|(_, _, kind)| *kind)
}

fn compose(first: &Pairs, second: &Pairs) -> Pairs {
    let mut out = Pairs::new();
    for (a, b) in first {
        for (b2, c) in second {
            if b == b2 {
                out.insert((a.clone(), c.clone()));
            }
        }
    }
    out
}

// ── Cv: converse of an existing relation ─────────────────────────────────────

/// Are the `located_in` pairs the converse of the `contains` pairs?
///
/// `contains` is parent -> child, so "parts of the lung" reads `contains` from lung. Under
/// the converse encoding a disorder located in the lung would be stored as a `contains`
/// child of the lung and appear among its parts.
fn cv_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let r = all_pairs(graph, R);
    let converse: Pairs = all_pairs(graph, "contains")
        .into_iter()
        .map(|(parent, child)| (child, parent))
        .collect();
    if !r.is_empty() && r == converse {
        EliminatorCheck::Eliminated {
            shared_answer: fmt_pairs(&r),
            reason: "located_in pairs equal the converse of contains".to_string(),
        }
    } else {
        EliminatorCheck::Passes {
            cheaper: fmt_pairs(&converse),
            r_answer: fmt_pairs(&r),
        }
    }
}

const CV: Fixture = Fixture {
    eliminator: "Cv",
    cheaper_encoding: "converse of contains (a located thing stored as a child of its location)",
    graph: &[
        ("lung", "contains", "right_lung"),
        ("pneumonia", "located_in", "lung"),
    ],
    query: "what are the parts of the lung, and what is located in it?",
    check: cv_check,
};

// ── Er: endpoint restriction of an existing relation ─────────────────────────

/// Are the `located_in` pairs `part_of` restricted to disorder -> anatomy endpoints?
///
/// The fixture carries `lung located_in thoracic_cavity`: anatomy -> anatomy, outside the
/// restriction, and an organ occupies a space it does not constitute. Encoding it as
/// unrestricted `part_of` would make the lung a constituent of a space.
fn er_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let r = all_pairs(graph, R);
    let restricted: Pairs = all_pairs(graph, "part_of")
        .into_iter()
        .filter(|(s, t)| {
            kind_of(graph, s) == Some("disorder") && kind_of(graph, t) == Some("anatomy")
        })
        .collect();
    if !r.is_empty() && r == restricted {
        EliminatorCheck::Eliminated {
            shared_answer: fmt_pairs(&r),
            reason: "located_in equals part_of restricted to disorder -> anatomy".to_string(),
        }
    } else {
        EliminatorCheck::Passes {
            cheaper: fmt_pairs(&restricted),
            r_answer: fmt_pairs(&r),
        }
    }
}

const ER: Fixture = Fixture {
    eliminator: "Er",
    cheaper_encoding: "part_of restricted to disorder -> anatomy endpoints",
    graph: &[
        ("lung", "kind", "anatomy"),
        ("respiratory_system", "kind", "anatomy"),
        ("right_lung", "kind", "anatomy"),
        ("thoracic_cavity", "kind", "anatomy"),
        ("pneumonia", "kind", "disorder"),
        ("right_lung", "part_of", "lung"),
        ("lung", "part_of", "respiratory_system"),
        ("pneumonia", "located_in", "lung"),
        ("lung", "located_in", "thoracic_cavity"),
    ],
    query: "what occupies the thoracic cavity, and what is in the lung?",
    check: er_check,
};

// ── At: existing relation plus a metadata attribute value ────────────────────

/// Would a reader of `part_of` that ignores `role = location` see the same answer?
///
/// The hypothesis stores location as `part_of` carrying `metadata.role = location`.
/// Every generic closure over `part_of` today reads the relation without filtering on the
/// attribute, so the answer it gives is every `part_of` pair; the encoding is redundant
/// only if that unfiltered answer equals the `located_in` answer.
fn at_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let r = all_pairs(graph, R);
    let unfiltered = all_pairs(graph, "part_of");
    if !r.is_empty() && r == unfiltered {
        EliminatorCheck::Eliminated {
            shared_answer: fmt_pairs(&r),
            reason: "an attribute-blind reader of part_of returns exactly the located_in pairs"
                .to_string(),
        }
    } else {
        EliminatorCheck::Passes {
            cheaper: fmt_pairs(&unfiltered),
            r_answer: fmt_pairs(&r),
        }
    }
}

const AT: Fixture = Fixture {
    eliminator: "At",
    cheaper_encoding: "part_of with metadata {role: 'location'}, read without the attribute filter",
    graph: &[
        ("right_lung", "part_of", "lung"),
        ("lung", "part_of", "respiratory_system"),
        ("pneumonia", "located_in", "lung"),
        ("pneumonia", "attr:role:location", "lung"),
    ],
    query: "what are the parts of the respiratory system?",
    check: at_check,
};

// ── Po: polarity partition of an existing relation ───────────────────────────

/// Do `located_in` and some existing relation partition a third existing relation?
///
/// No existing relation is the negation of location, so this eliminator is defeated
/// vacuously and recorded as such: the check searches the graph for any existing pair of
/// relations (E, B) with `located_in` ∪ E = B and `located_in` ∩ E = ∅, and finds none.
fn po_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let r = all_pairs(graph, R);
    let relations = existing_relations(graph);
    for opposite in &relations {
        let o = all_pairs(graph, opposite);
        if r.is_empty() || !r.is_disjoint(&o) {
            continue;
        }
        let union: Pairs = r.union(&o).cloned().collect();
        for base in &relations {
            if base != opposite && all_pairs(graph, base) == union {
                return EliminatorCheck::Eliminated {
                    shared_answer: fmt_pairs(&union),
                    reason: format!("located_in and {opposite} partition {base} exactly"),
                };
            }
        }
    }
    EliminatorCheck::Passes {
        cheaper: "(no existing relation is a negation of location)".to_string(),
        r_answer: fmt_pairs(&r),
    }
}

const PO: Fixture = Fixture {
    eliminator: "Po",
    cheaper_encoding: "an existing relation with a polarity attribute",
    graph: &[
        ("right_lung", "part_of", "lung"),
        ("lung", "part_of", "respiratory_system"),
        ("lung", "contains", "right_lung"),
        ("pneumonia", "located_in", "lung"),
    ],
    query: "is there an existing relation whose negation is location?",
    check: po_check,
};

// ── Ch: fixed property chain of existing relations ───────────────────────────

/// Is every `located_in` pair derivable as a two-step chain of existing relations?
fn ch_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let r = all_pairs(graph, R);
    let relations: Vec<&'static str> = existing_relations(graph).into_iter().collect();
    let mut derivable = Pairs::new();
    for first in &relations {
        for second in &relations {
            derivable.extend(compose(&all_pairs(graph, first), &all_pairs(graph, second)));
        }
    }
    if !r.is_empty() && r.is_subset(&derivable) {
        EliminatorCheck::Eliminated {
            shared_answer: fmt_pairs(&r),
            reason: "every located_in pair is a chain of two existing relations".to_string(),
        }
    } else {
        EliminatorCheck::Passes {
            cheaper: fmt_pairs(&derivable),
            r_answer: fmt_pairs(&r),
        }
    }
}

const CH: Fixture = Fixture {
    eliminator: "Ch",
    cheaper_encoding: "a composition of two existing relations",
    graph: &[
        ("right_lung", "part_of", "lung"),
        ("lung", "part_of", "respiratory_system"),
        ("lung", "contains", "right_lung"),
        ("pneumonia", "located_in", "lung"),
    ],
    query: "which composition of existing relations derives pneumonia -> lung?",
    check: ch_check,
};

// ── Mv: materialized reachability view over existing relations ───────────────

/// Are the `located_in` pairs the reachability closure of one existing relation?
fn mv_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let r = all_pairs(graph, R);
    let sources: BTreeSet<String> = r.iter().map(|(s, _)| s.clone()).collect();
    for rel in existing_relations(graph) {
        let view: Pairs = sources
            .iter()
            .flat_map(|s| reachable(graph, s, rel).into_iter().map(|t| (s.clone(), t)))
            .collect();
        if !r.is_empty() && r == view {
            return EliminatorCheck::Eliminated {
                shared_answer: fmt_pairs(&r),
                reason: format!("located_in equals the {rel} reachability closure"),
            };
        }
    }
    EliminatorCheck::Passes {
        cheaper: "(no existing relation's reachability view equals the located_in pairs)"
            .to_string(),
        r_answer: fmt_pairs(&r),
    }
}

const MV: Fixture = Fixture {
    eliminator: "Mv",
    cheaper_encoding: "a materialized reachability view over one existing relation",
    graph: &[
        ("right_lung", "part_of", "lung"),
        ("lung", "part_of", "respiratory_system"),
        ("lung", "contains", "right_lung"),
        ("pneumonia", "located_in", "lung"),
    ],
    query: "what is reachable from pneumonia over existing relations?",
    check: mv_check,
};

// ── Sr: typed sub-relation of a broader parent ───────────────────────────────

/// Are all `located_in` pairs a subset of one existing relation's pairs?
///
/// The nearest parent by name is `links_to` (a document references another document); its
/// meaning is false for pneumonia -> lung, and a reader of the parent would draw a false
/// conclusion.
fn sr_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let r = all_pairs(graph, R);
    for parent in existing_relations(graph) {
        if !r.is_empty() && r.is_subset(&all_pairs(graph, parent)) {
            return EliminatorCheck::Eliminated {
                shared_answer: fmt_pairs(&r),
                reason: format!("located_in pairs are a subset of {parent}"),
            };
        }
    }
    EliminatorCheck::Passes {
        cheaper: "(no existing relation contains every located_in pair)".to_string(),
        r_answer: fmt_pairs(&r),
    }
}

const SR: Fixture = Fixture {
    eliminator: "Sr",
    cheaper_encoding: "typed sub-relation of links_to (or of any other existing relation)",
    graph: &[
        ("doc_a", "links_to", "doc_b"),
        ("right_lung", "part_of", "lung"),
        ("pneumonia", "located_in", "lung"),
    ],
    query: "what does pneumonia point at?",
    check: sr_check,
};

// ── Positive tests ───────────────────────────────────────────────────────────

/// The seven positive fixtures, exported for the closed-set coverage gate.
pub const FIXTURES: &[Fixture] = &[CV, ER, AT, PO, CH, MV, SR];

#[test]
fn cv_located_in_is_not_converse_of_contains() {
    assert_defeats_eliminator(R, &CV);
}

#[test]
fn er_located_in_is_not_part_of_restricted_to_disorder_anatomy() {
    assert_defeats_eliminator(R, &ER);
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

#[test]
fn full_certificate_located_in_defeats_all_seven_eliminators() {
    run_certificate(R, FIXTURES);
}

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
