// ── Ch — fixed property chain (composition) of existing relations ─────────────

/// Check: are `cites` pairs equal to the shared-concept chain of `introduced_by`?
///
/// Chain: for each concept C, pairs (A, B) where introduced_by(C, A) and introduced_by(C, B).
/// Positive graph: white_paper cites spec_doc; concept_y introduced_by paper_x;
/// concept_y introduced_by paper_z.
/// `cites` = {(white_paper, spec_doc)}, chain = {(paper_x, paper_z), (paper_z, paper_x)}.
/// Different → Passes.
fn ch_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let r = all_pairs(graph, "cites");
    let intro = all_pairs(graph, "introduced_by");
    let concepts: BTreeSet<String> = intro.iter().map(|(c, _)| c.clone()).collect();
    let mut chain: BTreeSet<(String, String)> = BTreeSet::new();
    for concept in &concepts {
        let papers: Vec<String> = intro
            .iter()
            .filter(|(c, _)| c == concept)
            .map(|(_, p)| p.clone())
            .collect();
        for pa in &papers {
            for pb in &papers {
                if pa != pb {
                    chain.insert((pa.clone(), pb.clone()));
                }
            }
        }
    }
    if r == chain {
        EliminatorCheck::Eliminated {
            shared_answer: fmt_pairs(&r),
            reason: "cites equals the shared-concept chain of introduced_by".to_string(),
        }
    } else {
        EliminatorCheck::Passes {
            cheaper: fmt_pairs(&chain),
            r_answer: fmt_pairs(&r),
        }
    }
}

const CH: Fixture = Fixture {
    eliminator: "Ch",
    cheaper_encoding: "exists C: introduced_by(C,A) and introduced_by(C,B) (shared-concept chain)",
    graph: &[
        ("white_paper", "cites", "spec_doc"),
        ("concept_y", "introduced_by", "paper_x"),
        ("concept_y", "introduced_by", "paper_z"),
    ],
    query: "what does white_paper cite?",
    check: ch_check,
};

#[test]
fn ch_cites_is_not_a_property_chain_of_existing_relations() {
    assert_defeats_eliminator("cites", &CH);
}

/// Negative control: `builds_on_concept_from` IS the implements∘introduced_by chain.
///
/// Graph: project_x implements concept_raft; concept_raft introduced_by paper_raft;
/// project_x builds_on_concept_from paper_raft.
/// Chain(implements, introduced_by) = {(project_x, paper_raft)} = R → Eliminated.
fn ch_neg_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let r = all_pairs(graph, "builds_on_concept_from");
    let e1 = all_pairs(graph, "implements");
    let e2 = all_pairs(graph, "introduced_by");
    let mut chain: BTreeSet<(String, String)> = BTreeSet::new();
    for (a, c) in &e1 {
        for (c2, b) in &e2 {
            if c == c2 {
                chain.insert((a.clone(), b.clone()));
            }
        }
    }
    if r == chain {
        EliminatorCheck::Eliminated {
            shared_answer: fmt_pairs(&r),
            reason: "builds_on_concept_from equals implements composed with introduced_by"
                .to_string(),
        }
    } else {
        EliminatorCheck::Passes {
            cheaper: fmt_pairs(&chain),
            r_answer: fmt_pairs(&r),
        }
    }
}

const CH_NEG: Fixture = Fixture {
    eliminator: "Ch",
    cheaper_encoding: "implements composed with introduced_by (two-hop chain)",
    graph: &[
        ("project_x", "implements", "concept_raft"),
        ("concept_raft", "introduced_by", "paper_raft"),
        ("project_x", "builds_on_concept_from", "paper_raft"),
    ],
    query: "are builds_on_concept_from pairs equal to the implements-then-introduced_by chain?",
    check: ch_neg_check,
};

#[test]
fn ch_builds_on_concept_from_is_rejected_as_property_chain() {
    assert_eliminated_by("builds_on_concept_from", &CH_NEG);
}

// ── Mv — materialized reachability view over existing relations ───────────────

/// Check: are `cites` pairs equal to the derived_from transitive closure from each source?
///
/// Positive graph: thesis_2024 cites landmark_1972; thesis_2024 also has a derived_from
/// chain to interim_2020 and (transitively) base_2018 — reachability view is non-empty.
/// cites = {(thesis_2024, landmark_1972)}, derived_from reachability from thesis_2024
/// = {(thesis_2024, interim_2020), (thesis_2024, base_2018)} — non-empty.
/// cites ≠ reachability → Passes non-vacuously: a citation is a direct reference to an
/// intellectual source, not the transitive provenance chain of the citing document's own
/// derivation — encoding citations as reachability would yield entirely wrong pairs.
fn mv_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let r = all_pairs(graph, "cites");
    let sources: BTreeSet<String> = r.iter().map(|(s, _)| s.clone()).collect();
    let reachability: BTreeSet<(String, String)> = sources
        .iter()
        .flat_map(|s| {
            reachable(graph, s, "derived_from")
                .into_iter()
                .map(|t| (s.clone(), t))
        })
        .collect();
    if r == reachability {
        EliminatorCheck::Eliminated {
            shared_answer: fmt_pairs(&r),
            reason: "cites equals the derived_from transitive closure".to_string(),
        }
    } else {
        EliminatorCheck::Passes {
            cheaper: fmt_pairs(&reachability),
            r_answer: fmt_pairs(&r),
        }
    }
}

const MV: Fixture = Fixture {
    eliminator: "Mv",
    cheaper_encoding: "materialized reachability view over derived_from paths",
    graph: &[
        ("thesis_2024", "cites", "landmark_1972"),
        // Real derived_from chain: thesis derived from interim_2020, which derived from base_2018.
        // Reachability from thesis_2024 = {interim_2020, base_2018} — non-empty.
        // cites targets landmark_1972; reachability targets {interim_2020, base_2018}: wrong pairs.
        ("thesis_2024", "derived_from", "interim_2020"),
        ("interim_2020", "derived_from", "base_2018"),
    ],
    query: "what is reachable from thesis_2024 via derived_from?",
    check: mv_check,
};

#[test]
fn mv_cites_is_not_a_materialized_view_of_derived_from_reachability() {
    assert_defeats_eliminator("cites", &MV);
}

/// Negative control: `transitively_derived_from` IS the derived_from transitive closure.
///
/// Graph: artifact_a derived_from artifact_b; artifact_b derived_from artifact_c;
/// artifact_a transitively_derived_from artifact_b; artifact_a transitively_derived_from artifact_c.
/// Reachability from artifact_a = {artifact_b, artifact_c} = R pairs → Eliminated.
fn mv_neg_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let r = all_pairs(graph, "transitively_derived_from");
    let sources: BTreeSet<String> = r.iter().map(|(s, _)| s.clone()).collect();
    let reachability: BTreeSet<(String, String)> = sources
        .iter()
        .flat_map(|s| {
            reachable(graph, s, "derived_from")
                .into_iter()
                .map(|t| (s.clone(), t))
        })
        .collect();
    if r == reachability {
        EliminatorCheck::Eliminated {
            shared_answer: fmt_pairs(&r),
            reason: "transitively_derived_from equals the derived_from transitive closure"
                .to_string(),
        }
    } else {
        EliminatorCheck::Passes {
            cheaper: fmt_pairs(&reachability),
            r_answer: fmt_pairs(&r),
        }
    }
}

const MV_NEG: Fixture = Fixture {
    eliminator: "Mv",
    cheaper_encoding: "transitive closure of derived_from",
    graph: &[
        ("artifact_a", "derived_from", "artifact_b"),
        ("artifact_b", "derived_from", "artifact_c"),
        ("artifact_a", "transitively_derived_from", "artifact_b"),
        ("artifact_a", "transitively_derived_from", "artifact_c"),
    ],
    query: "are transitively_derived_from pairs equal to the derived_from closure?",
    check: mv_neg_check,
};

#[test]
fn mv_transitively_derived_from_is_rejected_as_reachability_view() {
    assert_eliminated_by("transitively_derived_from", &MV_NEG);
}
