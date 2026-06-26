//! ADR-076 non-redundancy certificate fixtures for the `cites` relation.
//!
//! `cites` is proposed as a Tier-1 core relation for directed intellectual
//! reference: a document references another document (or concept) as a source.
//! Wire direction: `document → document` (primary), `document → concept`.
//!
//! Each numbered test covers one eliminator from ADR-076 §D2. The final test
//! (`full_certificate`) runs all seven together to verify completeness.

use crate::harness::{assert_defeats_eliminator, run_certificate, Fixture, GraphTriple};

// ── fixture constructor ──────────────────────────────────────────────────────

const fn make(
    eliminator: &'static str,
    cheaper_encoding: &'static str,
    graph: &'static [GraphTriple],
    query: &'static str,
    cheaper_answer: &'static str,
    r_answer: &'static str,
) -> Fixture {
    Fixture {
        eliminator,
        cheaper_encoding,
        graph,
        query,
        cheaper_answer,
        r_answer,
    }
}

// ── Cv — converse of an existing relation ───────────────────────────────────

/// `cites(A, B)` is NOT the converse of `introduced_by`.
///
/// `introduced_by` goes `concept → document | person`. Its converse at a
/// document node B (`direction=in`) returns the concepts introduced in B,
/// not the papers that cited B. Querying who cited paper_b with
/// `direction=in` on `introduced_by` returns concept_c — the wrong kind
/// and the wrong answer.
const CV: Fixture = make(
    "Cv",
    "direction=in on introduced_by at the target document",
    &[
        // paper_a cites paper_b (the relation under test)
        ("paper_a", "cites", "paper_b"),
        // concept_c was introduced in paper_b (normal introduced_by direction)
        ("concept_c", "introduced_by", "paper_b"),
    ],
    "what edges point at paper_b in the cites direction?",
    // cheaper: incoming introduced_by at paper_b returns concept_c (wrong subject)
    "concept_c (via introduced_by incoming)",
    // R: incoming cites at paper_b returns paper_a
    "paper_a (via cites incoming)",
);

#[test]
fn cv_cites_is_not_converse_of_introduced_by() {
    assert_defeats_eliminator("cites", &CV);
}

// ── Er — endpoint restriction of an existing relation ───────────────────────

/// `cites` is NOT `derived_from` restricted to `document → document`.
///
/// `derived_from` denotes artifact provenance (a dataset or model produced
/// from another). A survey paper cites foundational work without being an
/// artifact derived from it. The endpoint restriction alone does not bridge
/// the semantic gap: `derived_from(document→document)` would claim the citing
/// paper was produced as an output of the cited paper, which is false.
const ER: Fixture = make(
    "Er",
    "derived_from restricted to document-to-document pairs",
    &[("survey_2024", "cites", "foundation_2010")],
    "what does survey_2024 reference?",
    // cheaper: no derived_from edge exists — survey is not produced from foundation_2010
    "nothing (survey_2024 was not derived from foundation_2010)",
    "foundation_2010",
);

#[test]
fn er_cites_is_not_derived_from_restricted_to_documents() {
    assert_defeats_eliminator("cites", &ER);
}

// ── At — existing relation plus a metadata attribute value ──────────────────

/// `cites` cannot be encoded as `introduced_by + {role: "citation"}`.
///
/// `introduced_by` requires a concept or artifact as source; a document
/// source violates the base endpoint contract. Even setting endpoint rules
/// aside, edge metadata is populated on 3.3% of edges (ADR-076 empirical
/// section), so a query filtering on `metadata.role="citation"` would miss
/// citations that carry no attribute. The cheaper encoding cannot represent
/// a document-to-document reference at all.
const AT: Fixture = make(
    "At",
    "introduced_by + metadata {role: 'citation'} on document-to-document pair",
    &[
        // tr_7 cites rfc_2119 (both documents)
        ("tr_7", "cites", "rfc_2119"),
        // rfc_2119 introduced a concept (the valid introduced_by direction)
        ("concept_must_keyword", "introduced_by", "rfc_2119"),
    ],
    "what does tr_7 cite?",
    // cheaper: document→document introduced_by violates endpoint contract; answer is empty
    "nothing (document source violates introduced_by endpoint contract)",
    "rfc_2119",
);

#[test]
fn at_cites_is_not_introduced_by_with_role_citation() {
    assert_defeats_eliminator("cites", &AT);
}

// ── Po — existing relation plus a polarity / sign attribute ─────────────────

/// `cites` cannot be encoded as `supports` or `refutes` (polarity pair).
///
/// A citation may be neutral — a paper cites a methods section as background
/// without endorsing or contesting its conclusions. Encoding neutral citations
/// as `supports` or `refutes` misclassifies the edge as making an epistemic
/// claim. The cheaper polarity pair forces a binary choice where none exists;
/// `cites` carries no polarity.
const PO: Fixture = make(
    "Po",
    "supports or refutes with a polarity attribute (assesses + polarity)",
    &[
        // paper_a cites paper_b as a neutral methodological reference
        ("paper_a", "cites", "paper_b"),
    ],
    "what is the epistemic stance of paper_a toward paper_b?",
    // cheaper: must pick supports or refutes; neutral citation is unrepresentable
    "supports (forced; neutral methodological citations cannot be encoded)",
    // R: cites carries no epistemic polarity — it is a reference, not a claim
    "no polarity (cites is a reference, not an epistemic stance)",
);

#[test]
fn po_cites_is_not_supports_or_refutes_with_polarity() {
    assert_defeats_eliminator("cites", &PO);
}

// ── Ch — fixed property chain (composition) of existing relations ────────────

/// `cites` is NOT a property chain of existing relations.
///
/// Chain hypothesis: `cites(A, B)` = exists concept C such that
/// `introduced_by(C, A)` and `introduced_by(C, B)` (shared-concept path).
/// The chain admits (A, B) when they share an introduced concept, but A may
/// cite B with no shared concept intermediary. Conversely, the chain admits
/// pairs that `cites` rejects (two papers sharing a concept need not cite
/// each other). The fixture shows both failures simultaneously.
const CH: Fixture = make(
    "Ch",
    "exists C: introduced_by(C,A) and introduced_by(C,B) (shared-concept chain)",
    &[
        // Direct citation with no chain path
        ("white_paper", "cites", "spec_doc"),
        // Chain path that does NOT produce a cites edge
        ("concept_y", "introduced_by", "paper_x"),
        ("concept_y", "introduced_by", "paper_z"),
        // paper_x and paper_z share concept_y but do not cite each other
    ],
    "what does white_paper cite?",
    // cheaper (chain): white_paper has no outgoing introduced_by; chain returns nothing
    "nothing (no shared-concept chain from white_paper to spec_doc)",
    "spec_doc",
);

#[test]
fn ch_cites_is_not_a_property_chain_of_existing_relations() {
    assert_defeats_eliminator("cites", &CH);
}

// ── Mv — materialized view (reachability) over existing relations ────────────

/// `cites` is NOT a materialized reachability view over `derived_from`.
///
/// A citation graph captures direct intellectual reference, not provenance
/// reachability. A thesis may cite a landmark paper from 50 years ago
/// without any `derived_from` path existing between them — the thesis was not
/// produced as an artifact derived from the landmark paper. The view and the
/// asserted relation diverge on every citation that lacks a provenance path.
const MV: Fixture = make(
    "Mv",
    "materialized reachability view over derived_from paths",
    &[
        // thesis_2024 cites landmark_1972 — direct intellectual reference
        ("thesis_2024", "cites", "landmark_1972"),
        // No derived_from path between thesis_2024 and landmark_1972
    ],
    "what is reachable from thesis_2024 via derived_from?",
    // cheaper: no derived_from edges from thesis_2024; view returns nothing
    "nothing (thesis_2024 has no derived_from edges)",
    "landmark_1972",
);

#[test]
fn mv_cites_is_not_a_materialized_view_of_derived_from_reachability() {
    assert_defeats_eliminator("cites", &MV);
}

// ── Sr — sub-relation of a broader parent ───────────────────────────────────

/// `cites` is NOT a typed sub-relation of `introduced_by`.
///
/// `introduced_by` goes `concept → document | person` (concept is the source).
/// `cites` goes `document → document` (document is the source). They have
/// opposite source endpoint kinds. Modeling `cites ⊑ introduced_by` would
/// require the parent to accept a document source, which widens its semantics
/// to conflate "this concept appeared in X" with "this document references X".
/// The base endpoint contract rejects `document --introduced_by-->` entirely,
/// so the cheaper sub-relation encoding cannot express any `cites` edge.
const SR: Fixture = make(
    "Sr",
    "typed sub-relation of introduced_by (cites subtype of introduced_by)",
    &[
        // design_doc cites spec_v3 (document → document)
        ("design_doc", "cites", "spec_v3"),
        // the valid introduced_by direction: concept introduced in spec_v3
        ("concept_http2", "introduced_by", "spec_v3"),
    ],
    "what does design_doc cite?",
    // cheaper: document source violates introduced_by endpoint contract
    "nothing (design_doc cannot be introduced_by source; endpoint contract violated)",
    "spec_v3",
);

#[test]
fn sr_cites_is_not_a_sub_relation_of_introduced_by() {
    assert_defeats_eliminator("cites", &SR);
}

// ── Full certificate ─────────────────────────────────────────────────────────

/// All seven eliminators together — the complete non-redundancy certificate.
///
/// Every eliminator must be covered and every fixture must defeat its
/// eliminator. A missing eliminator or a non-diverging fixture is a test
/// failure, blocking admission of the `cites` relation.
#[test]
fn full_certificate_cites_defeats_all_seven_eliminators() {
    run_certificate("cites", &[CV, ER, AT, PO, CH, MV, SR]);
}
