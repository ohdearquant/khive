// ── Cv — converse of an existing relation ────────────────────────────────────

/// Check: are the `cites` pairs equal to the CONVERSE of the `introduced_by` pairs?
///
/// Positive fixture graph: paper_a cites paper_b; concept_c introduced_by paper_b.
/// `cites` = {(paper_a, paper_b)}, converse(introduced_by) = {(paper_b, concept_c)}.
/// These differ → Passes.
fn cv_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let r = all_pairs(graph, "cites");
    let e = all_pairs(graph, "introduced_by");
    let converse_e: BTreeSet<(String, String)> = e.into_iter().map(|(a, b)| (b, a)).collect();
    if r == converse_e {
        EliminatorCheck::Eliminated {
            shared_answer: fmt_pairs(&r),
            reason: "cites pairs equal the converse of introduced_by".to_string(),
        }
    } else {
        EliminatorCheck::Passes {
            cheaper: fmt_pairs(&converse_e),
            r_answer: fmt_pairs(&r),
        }
    }
}

const CV: Fixture = Fixture {
    eliminator: "Cv",
    cheaper_encoding: "direction=in on introduced_by at the target document",
    graph: &[
        ("paper_a", "cites", "paper_b"),
        ("concept_c", "introduced_by", "paper_b"),
    ],
    query: "what edges point at paper_b in the cites direction?",
    check: cv_check,
};

#[test]
fn cv_cites_is_not_converse_of_introduced_by() {
    assert_defeats_eliminator("cites", &CV);
}

/// Negative control: `cited_by` IS the converse of `cites`.
///
/// Graph: paper_a cites paper_b; paper_b cited_by paper_a.
/// `cited_by` = {(paper_b, paper_a)}, converse(cites) = {(paper_b, paper_a)}.
/// Same → Eliminated.
fn cv_neg_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let r = all_pairs(graph, "cited_by");
    let e = all_pairs(graph, "cites");
    let converse_e: BTreeSet<(String, String)> = e.into_iter().map(|(a, b)| (b, a)).collect();
    if r == converse_e {
        EliminatorCheck::Eliminated {
            shared_answer: fmt_pairs(&r),
            reason: "cited_by pairs equal the converse of cites — redundant".to_string(),
        }
    } else {
        EliminatorCheck::Passes {
            cheaper: fmt_pairs(&converse_e),
            r_answer: fmt_pairs(&r),
        }
    }
}

const CV_NEG: Fixture = Fixture {
    eliminator: "Cv",
    cheaper_encoding: "converse of cites",
    graph: &[
        ("paper_a", "cites", "paper_b"),
        ("paper_b", "cited_by", "paper_a"),
    ],
    query: "are cited_by pairs the converse of cites pairs?",
    check: cv_neg_check,
};

#[test]
fn cv_cited_by_is_rejected_as_converse_of_cites() {
    assert_eliminated_by("cited_by", &CV_NEG);
}

// ── Er — endpoint restriction of an existing relation ────────────────────────

/// Check: are the `cites` pairs equal to `derived_from` restricted to document→document?
///
/// Positive graph: survey_2024 cites foundation_2010; survey_2024 also has a real
/// derived_from edge to prior_survey_2023 (a document→document derivation, NOT a citation).
/// restricted derived_from = {(survey_2024, prior_survey_2023)} — non-empty.
/// cites = {(survey_2024, foundation_2010)} ≠ restricted derived_from → Passes non-vacuously:
/// encoding citation as endpoint-restricted derived_from would yield the wrong pair set
/// (prior_survey_2023 is the derivation source; foundation_2010 is the cited intellectual source).
fn er_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let r = all_pairs(graph, "cites");
    let cheaper: BTreeSet<(String, String)> = graph
        .iter()
        .filter(|(s, rel, t)| {
            *rel == "derived_from"
                && graph
                    .iter()
                    .any(|(n, k, v)| *n == *s && *k == "kind" && *v == "document")
                && graph
                    .iter()
                    .any(|(n, k, v)| *n == *t && *k == "kind" && *v == "document")
        })
        .map(|(s, _, t)| (s.to_string(), t.to_string()))
        .collect();
    if r == cheaper {
        EliminatorCheck::Eliminated {
            shared_answer: fmt_pairs(&r),
            reason: "cites equals derived_from restricted to document→document".to_string(),
        }
    } else {
        EliminatorCheck::Passes {
            cheaper: fmt_pairs(&cheaper),
            r_answer: fmt_pairs(&r),
        }
    }
}

const ER: Fixture = Fixture {
    eliminator: "Er",
    cheaper_encoding: "derived_from restricted to document-to-document pairs",
    graph: &[
        ("survey_2024", "cites", "foundation_2010"),
        // Real doc→doc derived_from edge: survey was derived from a prior survey,
        // but it CITES foundation_2010 as the intellectual source — different pairs.
        ("survey_2024", "derived_from", "prior_survey_2023"),
        ("survey_2024", "kind", "document"),
        ("foundation_2010", "kind", "document"),
        ("prior_survey_2023", "kind", "document"),
    ],
    query: "what does survey_2024 reference?",
    check: er_check,
};

#[test]
fn er_cites_is_not_derived_from_restricted_to_documents() {
    assert_defeats_eliminator("cites", &ER);
}

/// Negative control: `doc_version_of` IS `precedes` restricted to document→document.
///
/// Graph: doc_v1 doc_version_of doc_v2; doc_v1 precedes doc_v2; both document-kind.
/// `doc_version_of` = {(doc_v1, doc_v2)}, restricted precedes = {(doc_v1, doc_v2)}.
/// Same → Eliminated.
fn er_neg_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let r = all_pairs(graph, "doc_version_of");
    let cheaper: BTreeSet<(String, String)> = graph
        .iter()
        .filter(|(s, rel, t)| {
            *rel == "precedes"
                && graph
                    .iter()
                    .any(|(n, k, v)| *n == *s && *k == "kind" && *v == "document")
                && graph
                    .iter()
                    .any(|(n, k, v)| *n == *t && *k == "kind" && *v == "document")
        })
        .map(|(s, _, t)| (s.to_string(), t.to_string()))
        .collect();
    if r == cheaper {
        EliminatorCheck::Eliminated {
            shared_answer: fmt_pairs(&r),
            reason: "doc_version_of equals precedes restricted to document→document".to_string(),
        }
    } else {
        EliminatorCheck::Passes {
            cheaper: fmt_pairs(&cheaper),
            r_answer: fmt_pairs(&r),
        }
    }
}

const ER_NEG: Fixture = Fixture {
    eliminator: "Er",
    cheaper_encoding: "precedes restricted to document-to-document pairs",
    graph: &[
        ("doc_v1", "doc_version_of", "doc_v2"),
        ("doc_v1", "precedes", "doc_v2"),
        ("doc_v1", "kind", "document"),
        ("doc_v2", "kind", "document"),
    ],
    query: "are doc_version_of pairs the same as precedes restricted to documents?",
    check: er_neg_check,
};

#[test]
fn er_doc_version_of_is_rejected_as_endpoint_restricted_precedes() {
    assert_eliminated_by("doc_version_of", &ER_NEG);
}
