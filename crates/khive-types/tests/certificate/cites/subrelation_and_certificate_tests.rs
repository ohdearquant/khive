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
