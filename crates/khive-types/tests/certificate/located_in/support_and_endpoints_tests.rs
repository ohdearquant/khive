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

/// Are the `located_in` pairs `extends` over the shared `concept -> concept` signature?
///
/// `extends` asserts intellectual lineage (a Derivation relation). Stored as `extends`,
/// `pneumonia located_in lung` would make a lineage reader list pneumonia, and through it
/// bacterial pneumonia, among the descendants of the lung, and `lung located_in
/// thoracic_cavity` would make an organ a derivative of a space. The fixture carries one
/// real lineage pair, so the restricted relation answers a different question.
fn er_extends_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let r = all_pairs(graph, R);
    let restricted: Pairs = all_pairs(graph, "extends")
        .into_iter()
        .filter(|(s, t)| {
            kind_of(graph, s) == Some("concept") && kind_of(graph, t) == Some("concept")
        })
        .collect();
    if !r.is_empty() && r == restricted {
        EliminatorCheck::Eliminated {
            shared_answer: fmt_pairs(&r),
            reason: "located_in equals extends over concept -> concept".to_string(),
        }
    } else {
        EliminatorCheck::Passes {
            cheaper: fmt_pairs(&restricted),
            r_answer: fmt_pairs(&r),
        }
    }
}

const ER_EXTENDS: Fixture = Fixture {
    eliminator: "Er",
    cheaper_encoding: "extends restricted to the shared concept -> concept signature",
    graph: &[
        ("lung", "kind", "concept"),
        ("thoracic_cavity", "kind", "concept"),
        ("pneumonia", "kind", "concept"),
        ("bacterial_pneumonia", "kind", "concept"),
        ("bacterial_pneumonia", "extends", "pneumonia"),
        ("pneumonia", "located_in", "lung"),
        ("lung", "located_in", "thoracic_cavity"),
    ],
    query: "what descends from pneumonia by lineage, and what is located in the lung?",
    check: er_extends_check,
};
