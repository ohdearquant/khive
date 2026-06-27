//! ADR-076 coverage gate: every EdgeRelation must have a certificate entry or
//! an explicit system-role exception.
//!
//! This test fails whenever `EdgeRelation::ALL` gains a new variant that has
//! neither passed the non-redundancy certificate nor been granted a system-role
//! exemption. A future PR cannot bypass the admission gate by simply adding a
//! variant to the enum and omitting a certificate module.

use khive_types::EdgeRelation;

/// Relations that have passed the ADR-076 non-redundancy certificate and are
/// eligible for promotion to `EdgeRelation::ALL`.
///
/// Add a relation here ONLY after its certificate module passes all seven
/// eliminator tests in `khive-types/tests/certificate/` AND the relation is
/// admitted to `EdgeRelation::ALL`.
const CERTIFIED_RELATIONS: &[&str] = &[];

/// Relations in `EdgeRelation::ALL` that are kept by declared system role rather
/// than by a non-redundancy certificate (ADR-076 §D3).
///
/// All 17 current relations are grandfathered at ADR-076 adoption: they exist by
/// system role; the certificate harness gates new additions only. Each entry
/// records a relation as having a declared role that supersedes the certificate.
const SYSTEM_ROLE_EXCEPTIONS: &[&str] = &[
    "contains",      // structural containment role
    "part_of",       // structural composition role
    "instance_of",   // taxonomic instantiation role
    "extends",       // derivation / refinement role
    "variant_of",    // derivation / alternative role
    "introduced_by", // provenance attribution role
    "supersedes",    // versioning / replacement role
    "derived_from",  // artifact provenance role
    "precedes",      // temporal ordering role
    "depends_on",    // dependency role
    "enables",       // enablement role
    "implements",    // implementation role
    "competes_with", // competitive lateral role
    "composed_with", // compositional lateral role
    "annotates",     // cross-substrate annotation role (only note→entity/note)
    "supports",      // ADR-055 epistemic role (kept with refutes as a symmetric pair)
    "refutes",       // ADR-055 epistemic role (kept with supports as a symmetric pair)
];

/// Every relation in `EdgeRelation::ALL` must appear in CERTIFIED_RELATIONS or
/// SYSTEM_ROLE_EXCEPTIONS.
///
/// A new relation in `EdgeRelation::ALL` must either:
/// - Pass all seven ADR-076 eliminator checks and appear in CERTIFIED_RELATIONS, or
/// - Hold a declared system role listed in SYSTEM_ROLE_EXCEPTIONS with a comment
///   citing the ADR that justifies it.
///
/// Failing this test means a relation was added to `EdgeRelation::ALL` without
/// going through the admission gate. Add a certificate module under
/// `khive-types/tests/certificate/` and list the relation in CERTIFIED_RELATIONS,
/// or add an ADR-justified entry to SYSTEM_ROLE_EXCEPTIONS.
#[test]
fn every_edge_relation_has_cert_entry_or_system_role_exception() {
    for rel in EdgeRelation::ALL {
        let name = rel.as_str();
        let certified = CERTIFIED_RELATIONS.contains(&name);
        let excepted = SYSTEM_ROLE_EXCEPTIONS.contains(&name);
        assert!(
            certified || excepted,
            "EdgeRelation '{name}' is in EdgeRelation::ALL but has neither a \
             non-redundancy certificate in CERTIFIED_RELATIONS nor a system-role \
             exception in SYSTEM_ROLE_EXCEPTIONS (ADR-076 §D2). To add a new \
             relation: run its certificate module to pass all seven eliminators and \
             add it to CERTIFIED_RELATIONS, or add an ADR-justified system-role \
             exception to SYSTEM_ROLE_EXCEPTIONS.",
        );
    }
}
