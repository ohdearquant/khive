//! ADR-076 non-redundancy certificate fixtures for the `cites` relation.
//!
//! `cites` is proposed as a Tier-1 core relation for directed intellectual
//! reference: a document references another document (or concept) as a source.
//! Wire direction: `document → document` (primary), `document → concept`.
//!
//! Each positive test (numbered) verifies that `cites` passes one eliminator
//! from ADR-076 §D2. Each negative-control test (suffix `_rejected_by_*`)
//! verifies that a deliberately-redundant candidate IS eliminated, proving the
//! check functions are not vacuous. The final test (`full_certificate_cites`)
//! runs all seven positive fixtures together.

use std::collections::BTreeSet;

use crate::harness::{
    all_pairs, assert_defeats_eliminator, assert_eliminated_by, fmt_pairs, reachable,
    run_certificate, EliminatorCheck, Fixture, GraphTriple,
};

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

// ── At — existing relation plus a metadata attribute value ───────────────────

/// Check: are `cites` pairs equal to `introduced_by` ∩ `attr:role:citation`?
///
/// Positive graph: tr_7 cites rfc_2119; concept_must introduced_by rfc_2119 AND
/// concept_must attr:role:citation rfc_2119 (the attribute-qualified subset is non-empty).
/// introduced_by ∩ attr:role:citation = {(concept_must, rfc_2119)} — non-empty.
/// cites = {(tr_7, rfc_2119)} ≠ attr-qualified subset → Passes non-vacuously:
/// encoding citations as attr-qualified introduced_by would mislabel concept_must→rfc_2119
/// as a citation while missing the actual citation tr_7→rfc_2119 — different subjects.
fn at_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let r = all_pairs(graph, "cites");
    let base = all_pairs(graph, "introduced_by");
    let attr = all_pairs(graph, "attr:role:citation");
    let cheaper: BTreeSet<(String, String)> = base.intersection(&attr).cloned().collect();
    if r == cheaper {
        EliminatorCheck::Eliminated {
            shared_answer: fmt_pairs(&r),
            reason: "cites equals introduced_by intersected with attr:role:citation".to_string(),
        }
    } else {
        EliminatorCheck::Passes {
            cheaper: fmt_pairs(&cheaper),
            r_answer: fmt_pairs(&r),
        }
    }
}

const AT: Fixture = Fixture {
    eliminator: "At",
    cheaper_encoding: "introduced_by + metadata {role: 'citation'} on document-to-document pair",
    graph: &[
        ("tr_7", "cites", "rfc_2119"),
        // concept_must was introduced_by rfc_2119 WITH a citation-role attribute — so the
        // attr-qualified subset is non-empty: {(concept_must, rfc_2119)}.  This ≠ cites:
        // introducing a concept and citing a document are different subjects and relations.
        ("concept_must", "introduced_by", "rfc_2119"),
        ("concept_must", "attr:role:citation", "rfc_2119"),
    ],
    query: "what does tr_7 cite?",
    check: at_check,
};

#[test]
fn at_cites_is_not_introduced_by_with_role_citation() {
    assert_defeats_eliminator("cites", &AT);
}

/// Negative control: `strongly_supports` IS `supports` ∩ `attr:weight:strong`.
///
/// Graph: finding_a strongly_supports claim_b; finding_a supports claim_b;
/// (finding_a, attr:weight:strong, claim_b).
/// `strongly_supports` = {(finding_a, claim_b)}, intersection = {(finding_a, claim_b)}.
/// Same → Eliminated.
fn at_neg_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let r = all_pairs(graph, "strongly_supports");
    let base = all_pairs(graph, "supports");
    let attr = all_pairs(graph, "attr:weight:strong");
    let cheaper: BTreeSet<(String, String)> = base.intersection(&attr).cloned().collect();
    if r == cheaper {
        EliminatorCheck::Eliminated {
            shared_answer: fmt_pairs(&r),
            reason: "strongly_supports equals supports intersected with attr:weight:strong"
                .to_string(),
        }
    } else {
        EliminatorCheck::Passes {
            cheaper: fmt_pairs(&cheaper),
            r_answer: fmt_pairs(&r),
        }
    }
}

const AT_NEG: Fixture = Fixture {
    eliminator: "At",
    cheaper_encoding: "supports + attr:weight:strong attribute marker",
    graph: &[
        ("finding_a", "strongly_supports", "claim_b"),
        ("finding_a", "supports", "claim_b"),
        ("finding_a", "attr:weight:strong", "claim_b"),
    ],
    query: "are strongly_supports pairs the intersection of supports and the weight:strong attr?",
    check: at_neg_check,
};

#[test]
fn at_strongly_supports_is_rejected_as_attribute_qualified_supports() {
    assert_eliminated_by("strongly_supports", &AT_NEG);
}

// ── Po — polarity partition of an existing relation ──────────────────────────

/// Check: do `cites` and `counter_cites` form a polarity partition of `supports`?
///
/// Positive graph: paper_a cites paper_b (neutral reference); paper_a also supports
/// claim_c (polarity machinery is present and active — supports is non-empty).
/// union (cites ∪ counter_cites) = {(paper_a, paper_b)}, base (supports) = {(paper_a, claim_c)}.
/// union ≠ base → Passes non-vacuously: a citation is a neutral document reference that
/// cannot be recovered as one polarity of the epistemic `supports` relation — citations
/// and epistemic stances address different objects (documents vs. claims).
fn po_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let r = all_pairs(graph, "cites");
    let opposite = all_pairs(graph, "counter_cites");
    let base = all_pairs(graph, "supports");
    let union: BTreeSet<(String, String)> = r.union(&opposite).cloned().collect();
    let intersection: BTreeSet<(String, String)> = r.intersection(&opposite).cloned().collect();
    if union == base && intersection.is_empty() {
        EliminatorCheck::Eliminated {
            shared_answer: fmt_pairs(&union),
            reason: "cites and counter_cites partition supports exactly".to_string(),
        }
    } else {
        EliminatorCheck::Passes {
            cheaper: format!("union={} base={}", fmt_pairs(&union), fmt_pairs(&base)),
            r_answer: fmt_pairs(&r),
        }
    }
}

const PO: Fixture = Fixture {
    eliminator: "Po",
    cheaper_encoding: "supports or refutes with a polarity attribute (assesses + polarity)",
    graph: &[
        ("paper_a", "cites", "paper_b"),
        // supports edge present and active — polarity machinery is non-empty.
        // paper_a supports claim_c (a specific claim), while citing paper_b (the document).
        // Partition union {(paper_a, paper_b)} ≠ supports {(paper_a, claim_c)}.
        ("paper_a", "supports", "claim_c"),
    ],
    query: "what is the epistemic stance of paper_a toward paper_b?",
    check: po_check,
};

#[test]
fn po_cites_is_not_supports_or_refutes_with_polarity() {
    assert_defeats_eliminator("cites", &PO);
}

/// Negative control: `positive_supports` and `negative_supports` DO partition `supports`.
///
/// Graph: positive_supports(A, claim_b); negative_supports(A, other_claim);
/// supports(A, claim_b); supports(A, other_claim).
/// union = {(A, claim_b), (A, other_claim)} = supports, disjoint → Eliminated.
fn po_neg_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let r = all_pairs(graph, "positive_supports");
    let opposite = all_pairs(graph, "negative_supports");
    let base = all_pairs(graph, "supports");
    let union: BTreeSet<(String, String)> = r.union(&opposite).cloned().collect();
    let intersection: BTreeSet<(String, String)> = r.intersection(&opposite).cloned().collect();
    if union == base && intersection.is_empty() {
        EliminatorCheck::Eliminated {
            shared_answer: fmt_pairs(&union),
            reason: "positive_supports and negative_supports partition supports exactly"
                .to_string(),
        }
    } else {
        EliminatorCheck::Passes {
            cheaper: format!("union={} base={}", fmt_pairs(&union), fmt_pairs(&base)),
            r_answer: fmt_pairs(&r),
        }
    }
}

const PO_NEG: Fixture = Fixture {
    eliminator: "Po",
    cheaper_encoding: "supports split into positive_supports and negative_supports by polarity",
    graph: &[
        ("paper_a", "positive_supports", "claim_b"),
        ("paper_a", "negative_supports", "other_claim"),
        ("paper_a", "supports", "claim_b"),
        ("paper_a", "supports", "other_claim"),
    ],
    query: "do positive_supports and negative_supports partition supports?",
    check: po_neg_check,
};

#[test]
fn po_positive_supports_is_rejected_as_polarity_partition_of_supports() {
    assert_eliminated_by("positive_supports", &PO_NEG);
}

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

// ── Sr — sub-relation of a broader parent ────────────────────────────────────

/// Check: are all `cites` pairs a subset of `introduced_by` pairs?
///
/// Positive graph: design_doc cites spec_v3; concept_http2 introduced_by spec_v3.
/// `cites` = {(design_doc, spec_v3)}, introduced_by = {(concept_http2, spec_v3)}.
/// (design_doc, spec_v3) ∉ introduced_by → not a subset → Passes.
fn sr_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let r = all_pairs(graph, "cites");
    let e = all_pairs(graph, "introduced_by");
    let is_sub = !r.is_empty() && r.is_subset(&e);
    if is_sub {
        EliminatorCheck::Eliminated {
            shared_answer: fmt_pairs(&r),
            reason: "cites is a proper sub-relation of introduced_by".to_string(),
        }
    } else {
        EliminatorCheck::Passes {
            cheaper: fmt_pairs(&e),
            r_answer: fmt_pairs(&r),
        }
    }
}

const SR: Fixture = Fixture {
    eliminator: "Sr",
    cheaper_encoding: "typed sub-relation of introduced_by (cites subtype of introduced_by)",
    graph: &[
        ("design_doc", "cites", "spec_v3"),
        ("concept_http2", "introduced_by", "spec_v3"),
    ],
    query: "what does design_doc cite?",
    check: sr_check,
};

#[test]
fn sr_cites_is_not_a_sub_relation_of_introduced_by() {
    assert_defeats_eliminator("cites", &SR);
}

/// Negative control: `weak_extends` IS a sub-relation of `extends`.
///
/// Graph: concept_a weak_extends concept_b; concept_a extends concept_b.
/// `weak_extends` = {(concept_a, concept_b)} ⊆ extends = {(concept_a, concept_b)} → Eliminated.
fn sr_neg_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let r = all_pairs(graph, "weak_extends");
    let e = all_pairs(graph, "extends");
    let is_sub = !r.is_empty() && r.is_subset(&e);
    if is_sub {
        EliminatorCheck::Eliminated {
            shared_answer: fmt_pairs(&r),
            reason: "weak_extends pairs are a subset of extends — redundant sub-relation"
                .to_string(),
        }
    } else {
        EliminatorCheck::Passes {
            cheaper: fmt_pairs(&e),
            r_answer: fmt_pairs(&r),
        }
    }
}

const SR_NEG: Fixture = Fixture {
    eliminator: "Sr",
    cheaper_encoding: "typed sub-relation of extends",
    graph: &[
        ("concept_a", "weak_extends", "concept_b"),
        ("concept_a", "extends", "concept_b"),
    ],
    query: "are weak_extends pairs a subset of extends pairs?",
    check: sr_neg_check,
};

#[test]
fn sr_weak_extends_is_rejected_as_sub_relation_of_extends() {
    assert_eliminated_by("weak_extends", &SR_NEG);
}

// ── Full certificate ──────────────────────────────────────────────────────────

/// All seven positive eliminators together — the complete non-redundancy certificate.
///
/// Every eliminator must be covered and every fixture must pass. A missing
/// eliminator or a failing check is a test failure, blocking admission.
#[test]
fn full_certificate_cites_defeats_all_seven_eliminators() {
    run_certificate("cites", &[CV, ER, AT, PO, CH, MV, SR]);
}
