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
