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
