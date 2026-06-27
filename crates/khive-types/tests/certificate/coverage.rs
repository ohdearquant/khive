//! ADR-076 coverage gate: every EdgeRelation must have a certificate entry or
//! an explicit system-role exception.
//!
//! Both admission paths are typed so neither can be satisfied by a bare string.
//! CERTIFIED_RELATIONS carries fixtures that run_certificate executes on every
//! entry — a relation cannot be added without an actually-passing certificate.
//! SYSTEM_ROLE_EXCEPTIONS requires written `adr` and `role` fields that the test
//! asserts are non-empty, closing the lazy third path (comment-only justification)
//! that the raw &[&str] form permitted.

use khive_types::EdgeRelation;

use crate::harness::{run_certificate, Fixture};

/// A relation admitted by passing the ADR-076 non-redundancy certificate.
///
/// `fixtures` is executed by run_certificate; the relation cannot enter this list
/// without all seven eliminators passing.
struct CertifiedRelation {
    relation: &'static str,
    fixtures: &'static [Fixture],
}

/// A relation grandfathered by declared system role (ADR-076 §D3).
///
/// Both `adr` and `role` must be non-empty; the test asserts this, so future
/// entries cannot bypass the gate with a bare-string add and a comment.
struct SystemRoleException {
    relation: &'static str,
    adr: &'static str,
    role: &'static str,
}

/// Relations admitted by passing the non-redundancy certificate.
///
/// Empty today — `cites` is a worked-example in cites.rs, not yet a variant of
/// EdgeRelation::ALL. Add entries here only after EdgeRelation::ALL gains the
/// variant and its certificate module passes all seven eliminators.
const CERTIFIED_RELATIONS: &[CertifiedRelation] = &[];

/// Relations in EdgeRelation::ALL kept by declared system role (ADR-076 §D3).
///
/// All 17 current relations are grandfathered at ADR-076 adoption.
const SYSTEM_ROLE_EXCEPTIONS: &[SystemRoleException] = &[
    SystemRoleException {
        relation: "contains",
        adr: "ADR-002",
        role: "structural containment role",
    },
    SystemRoleException {
        relation: "part_of",
        adr: "ADR-002",
        role: "structural composition role",
    },
    SystemRoleException {
        relation: "instance_of",
        adr: "ADR-002",
        role: "taxonomic instantiation role",
    },
    SystemRoleException {
        relation: "extends",
        adr: "ADR-002",
        role: "derivation / refinement role",
    },
    SystemRoleException {
        relation: "variant_of",
        adr: "ADR-002",
        role: "derivation / alternative role",
    },
    SystemRoleException {
        relation: "introduced_by",
        adr: "ADR-002",
        role: "provenance attribution role",
    },
    SystemRoleException {
        relation: "supersedes",
        adr: "ADR-002",
        role: "versioning / replacement role",
    },
    SystemRoleException {
        relation: "derived_from",
        adr: "ADR-002",
        role: "artifact provenance role",
    },
    SystemRoleException {
        relation: "precedes",
        adr: "ADR-002",
        role: "temporal ordering role",
    },
    SystemRoleException {
        relation: "depends_on",
        adr: "ADR-002",
        role: "dependency role",
    },
    SystemRoleException {
        relation: "enables",
        adr: "ADR-002",
        role: "enablement role",
    },
    SystemRoleException {
        relation: "implements",
        adr: "ADR-002",
        role: "implementation role",
    },
    SystemRoleException {
        relation: "competes_with",
        adr: "ADR-002",
        role: "competitive lateral role",
    },
    SystemRoleException {
        relation: "composed_with",
        adr: "ADR-002",
        role: "compositional lateral role",
    },
    SystemRoleException {
        relation: "annotates",
        adr: "ADR-002",
        role: "cross-substrate annotation role (only note-to-entity/note)",
    },
    SystemRoleException {
        relation: "supports",
        adr: "ADR-055",
        role: "epistemic role (kept with refutes as a symmetric pair)",
    },
    SystemRoleException {
        relation: "refutes",
        adr: "ADR-055",
        role: "epistemic role (kept with supports as a symmetric pair)",
    },
];

/// Every system-role exception must carry a non-empty ADR citation and role description.
///
/// A future entry that omits either field is caught here before it can mask a
/// genuine admission bypass.
#[test]
fn system_role_exceptions_have_non_empty_adr_and_role() {
    for entry in SYSTEM_ROLE_EXCEPTIONS {
        assert!(
            !entry.adr.is_empty(),
            "SYSTEM_ROLE_EXCEPTIONS entry '{}' has an empty `adr` field; \
             every exception must cite the ADR that justifies it",
            entry.relation,
        );
        assert!(
            !entry.role.is_empty(),
            "SYSTEM_ROLE_EXCEPTIONS entry '{}' has an empty `role` field; \
             every exception must declare a system role",
            entry.relation,
        );
    }
}

/// Every certified relation must pass the full non-redundancy certificate.
///
/// run_certificate asserts all seven eliminator families are covered and every
/// fixture passes. An entry in CERTIFIED_RELATIONS without passing fixtures
/// fails this test.
#[test]
fn certified_relations_pass_full_certificate() {
    for entry in CERTIFIED_RELATIONS {
        run_certificate(entry.relation, entry.fixtures);
    }
}

/// Every relation in EdgeRelation::ALL must appear in CERTIFIED_RELATIONS or
/// SYSTEM_ROLE_EXCEPTIONS.
///
/// A new relation in EdgeRelation::ALL must either pass all seven ADR-076
/// eliminator checks (CERTIFIED_RELATIONS) or hold a declared system role with
/// a written ADR citation (SYSTEM_ROLE_EXCEPTIONS). Failing this test means a
/// relation was added to EdgeRelation::ALL without going through the admission
/// gate.
#[test]
fn every_edge_relation_has_cert_entry_or_system_role_exception() {
    for rel in EdgeRelation::ALL {
        let name = rel.as_str();
        let certified = CERTIFIED_RELATIONS.iter().any(|e| e.relation == name);
        let excepted = SYSTEM_ROLE_EXCEPTIONS.iter().any(|e| e.relation == name);
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
