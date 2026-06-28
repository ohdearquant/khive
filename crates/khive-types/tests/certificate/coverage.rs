//! ADR-076 coverage gate: every EdgeRelation must have a certificate entry or
//! an explicit system-role exception.
//!
//! Both admission paths are typed so neither can be satisfied by a bare string.
//! CERTIFIED_RELATIONS carries fixtures that run_certificate executes on every
//! entry — a relation cannot be added without an actually-passing certificate.
//! SYSTEM_ROLE_EXCEPTIONS requires written `adr`, `role`, and `disposition`
//! fields that tests assert are non-empty/valid, closing the lazy path that
//! a comment-only justification would permit.
//!
//! The `disposition` field (ADR-076 §D2/§D3) records the certificate outcome for
//! every system-role exception.  A relation that fails an eliminator but is kept
//! by system role MUST name the eliminator family and state the justification in
//! `FailsEliminator { family, kept_because }`.  A relation that survived every
//! eliminator (or was grandfathered without a fixture run) uses `SurvivesAll`.
//! This makes it mechanically impossible to take the system-role override path
//! without recording the eliminator outcome, which is the load-bearing requirement
//! of ADR-076 §D3 ("the certificate is necessary and not sufficient; the
//! declaration is the arbiter").

use khive_types::EdgeRelation;

use crate::harness::{run_certificate, Fixture};

/// Records the certificate eliminator outcome for a system-role exception.
///
/// Per ADR-076 §D3, the system-role declaration is the arbiter even when a
/// relation fails an eliminator.  A maintainer taking the system-role path
/// must explicitly state whether the relation survived all eliminators
/// (`SurvivesAll`) or failed a specific one and why it is kept despite that
/// (`FailsEliminator`).
enum CertDisposition {
    /// The relation was not shown to be redundant by any of the seven ADR-076
    /// §D2 eliminators.  The system role is still required for admission (D1).
    SurvivesAll,
    /// The relation fails the named eliminator family but is kept by system
    /// role per ADR-076 §D3.
    FailsEliminator {
        /// One of the seven ADR-076 §D2 eliminator codes: "Cv","Er","At","Po","Ch","Mv","Sr".
        family: &'static str,
        /// The system-role justification for keeping the relation despite the
        /// eliminator failure.  Must be non-empty — ADR-076 §D3 requires an
        /// explicit declaration ("the declaration is the arbiter").
        kept_because: &'static str,
    },
}

/// A relation admitted by passing the ADR-076 non-redundancy certificate.
///
/// `fixtures` is executed by run_certificate; the relation cannot enter this list
/// without all seven eliminators passing.
struct CertifiedRelation {
    relation: &'static str,
    fixtures: &'static [Fixture],
}

/// A relation kept by declared system role (ADR-076 §D1/§D3).
///
/// `adr` and `role` must be non-empty; `disposition` must record whether the
/// relation survives all certificate eliminators or fails one and why it is
/// kept anyway.  Tests assert all three fields are valid so a future entry
/// cannot bypass the gate with a bare-string add and a comment.
struct SystemRoleException {
    relation: &'static str,
    adr: &'static str,
    role: &'static str,
    disposition: CertDisposition,
}

/// Relations admitted by passing the non-redundancy certificate.
///
/// Empty today — `cites` is a worked-example in cites.rs, not yet a variant of
/// EdgeRelation::ALL. Add entries here only after EdgeRelation::ALL gains the
/// variant and its certificate module passes all seven eliminators.
const CERTIFIED_RELATIONS: &[CertifiedRelation] = &[];

/// Relations in EdgeRelation::ALL kept by declared system role (ADR-076 §D1/§D3).
///
/// All 17 current relations are grandfathered at ADR-076 adoption.  The
/// `disposition` field records the certificate analysis: 15 relations use
/// `SurvivesAll` (no eliminator defeats them); `supports` and `refutes` use
/// `FailsEliminator { family: "Po", … }` — the load-bearing §D3 case showing
/// a relation may fail the certificate and still be kept by system role.
const SYSTEM_ROLE_EXCEPTIONS: &[SystemRoleException] = &[
    SystemRoleException {
        relation: "contains",
        adr: "ADR-002",
        role: "structural containment role",
        disposition: CertDisposition::SurvivesAll,
    },
    SystemRoleException {
        relation: "part_of",
        adr: "ADR-002",
        role: "structural composition role",
        disposition: CertDisposition::SurvivesAll,
    },
    SystemRoleException {
        relation: "instance_of",
        adr: "ADR-002",
        role: "taxonomic instantiation role",
        disposition: CertDisposition::SurvivesAll,
    },
    SystemRoleException {
        relation: "extends",
        adr: "ADR-002",
        role: "derivation / refinement role",
        disposition: CertDisposition::SurvivesAll,
    },
    SystemRoleException {
        relation: "variant_of",
        adr: "ADR-002",
        role: "derivation / alternative role",
        disposition: CertDisposition::SurvivesAll,
    },
    SystemRoleException {
        relation: "introduced_by",
        adr: "ADR-002",
        role: "provenance attribution role",
        disposition: CertDisposition::SurvivesAll,
    },
    SystemRoleException {
        relation: "supersedes",
        adr: "ADR-002",
        role: "versioning / replacement role",
        disposition: CertDisposition::SurvivesAll,
    },
    SystemRoleException {
        relation: "derived_from",
        adr: "ADR-002",
        role: "artifact provenance role",
        disposition: CertDisposition::SurvivesAll,
    },
    SystemRoleException {
        relation: "precedes",
        adr: "ADR-002",
        role: "temporal ordering role",
        disposition: CertDisposition::SurvivesAll,
    },
    SystemRoleException {
        relation: "depends_on",
        adr: "ADR-002",
        role: "dependency role",
        disposition: CertDisposition::SurvivesAll,
    },
    SystemRoleException {
        relation: "enables",
        adr: "ADR-002",
        role: "enablement role",
        disposition: CertDisposition::SurvivesAll,
    },
    SystemRoleException {
        relation: "implements",
        adr: "ADR-002",
        role: "implementation role",
        disposition: CertDisposition::SurvivesAll,
    },
    SystemRoleException {
        relation: "competes_with",
        adr: "ADR-002",
        role: "competitive lateral role",
        disposition: CertDisposition::SurvivesAll,
    },
    SystemRoleException {
        relation: "composed_with",
        adr: "ADR-002",
        role: "compositional lateral role",
        disposition: CertDisposition::SurvivesAll,
    },
    SystemRoleException {
        relation: "annotates",
        adr: "ADR-002",
        role: "cross-substrate annotation role (only note-to-entity/note)",
        disposition: CertDisposition::SurvivesAll,
    },
    SystemRoleException {
        relation: "supports",
        adr: "ADR-055",
        role: "epistemic role (kept with refutes as a symmetric pair)",
        // ADR-076 §D3: `supports` fails the Po eliminator — a single `assesses`
        // relation carrying a polarity attribute answers every query the pair
        // answers.  It is kept because the epistemic layer requires polarity to
        // be a first-class, relation-level distinction that planners, indexes,
        // federation, and the public API can branch on directly, not a value
        // buried in an open metadata blob that 3.3% of edges populate.
        disposition: CertDisposition::FailsEliminator {
            family: "Po",
            kept_because: "the epistemic layer requires polarity to be a \
                           first-class, relation-level distinction that \
                           planners, indexes, federation, and the public API \
                           can branch on directly, not a value buried in an \
                           open metadata blob that 3.3% of edges populate \
                           (ADR-055; ADR-076 §D3)",
        },
    },
    SystemRoleException {
        relation: "refutes",
        adr: "ADR-055",
        role: "epistemic role (kept with supports as a symmetric pair)",
        // ADR-076 §D3: `refutes` fails the Po eliminator for the same reason
        // as `supports` — the pair is kept together so the distinction is
        // first-class at the relation level, not an attribute value.
        disposition: CertDisposition::FailsEliminator {
            family: "Po",
            kept_because: "the epistemic layer requires polarity to be a \
                           first-class, relation-level distinction that \
                           planners, indexes, federation, and the public API \
                           can branch on directly, not a value buried in an \
                           open metadata blob that 3.3% of edges populate \
                           (ADR-055; ADR-076 §D3)",
        },
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

/// Every FailsEliminator disposition must name a valid eliminator family and
/// carry a non-empty justification.
///
/// ADR-076 §D3: a relation that fails an eliminator but is kept by system role
/// MUST declare which eliminator it fails and why it is kept anyway.  The
/// family must be one of the seven ADR-076 §D2 codes, and `kept_because` must
/// be non-empty ("the declaration is the arbiter").
#[test]
fn fail_eliminator_dispositions_have_valid_family_and_non_empty_justification() {
    const VALID_CODES: &[&str] = &["Cv", "Er", "At", "Po", "Ch", "Mv", "Sr"];
    for entry in SYSTEM_ROLE_EXCEPTIONS {
        if let CertDisposition::FailsEliminator {
            family,
            kept_because,
        } = &entry.disposition
        {
            assert!(
                VALID_CODES.contains(family),
                "SYSTEM_ROLE_EXCEPTIONS entry '{}' has FailsEliminator with unknown \
                 family '{}'; family must be one of {:?} (ADR-076 §D2 eliminator codes)",
                entry.relation,
                family,
                VALID_CODES,
            );
            assert!(
                !kept_because.is_empty(),
                "SYSTEM_ROLE_EXCEPTIONS entry '{}' has FailsEliminator with an empty \
                 `kept_because`; a failing eliminator MUST carry a system-role \
                 justification (ADR-076 §D3: the declaration is the arbiter)",
                entry.relation,
            );
        }
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

/// The set of relations with a `SurvivesAll` disposition equals exactly the 15 grandfathered
/// base relations (ADR-002, grandfathered at ADR-076 adoption).
///
/// A non-grandfathered relation added to `SYSTEM_ROLE_EXCEPTIONS` with `SurvivesAll` would
/// silently skip the certificate fixture path without any check running.  This assertion
/// makes such an addition mechanically visible: any new `SurvivesAll` entry must appear in
/// this closed list, which forces a reviewer to update the assertion rather than just appending
/// a struct literal.  Non-grandfathered relations must go through the certificate admission
/// path (CERTIFIED_RELATIONS) or declare `FailsEliminator` naming the eliminator and the
/// system-role justification.
#[test]
fn survives_all_disposition_is_exactly_the_15_grandfathered_base_relations() {
    const EXPECTED: &[&str] = &[
        "annotates",
        "competes_with",
        "composed_with",
        "contains",
        "depends_on",
        "derived_from",
        "enables",
        "extends",
        "implements",
        "instance_of",
        "introduced_by",
        "part_of",
        "precedes",
        "supersedes",
        "variant_of",
    ];

    let mut actual: Vec<&str> = SYSTEM_ROLE_EXCEPTIONS
        .iter()
        .filter(|e| matches!(e.disposition, CertDisposition::SurvivesAll))
        .map(|e| e.relation)
        .collect();
    actual.sort_unstable();

    assert_eq!(
        actual, EXPECTED,
        "the set of relations with SurvivesAll disposition must equal exactly the 15 \
         grandfathered ADR-002 base relations; adding a new SurvivesAll entry requires \
         updating this closed list — non-grandfathered relations must go through the \
         certificate admission path (CERTIFIED_RELATIONS) or declare FailsEliminator \
         with a valid family code and a non-empty kept_because justification"
    );
}

/// Every relation in EdgeRelation::ALL must appear in CERTIFIED_RELATIONS or
/// SYSTEM_ROLE_EXCEPTIONS.
///
/// A new relation in EdgeRelation::ALL must either pass all seven ADR-076
/// eliminator checks (CERTIFIED_RELATIONS) or hold a declared system role with
/// a written ADR citation and a stated certificate disposition
/// (SYSTEM_ROLE_EXCEPTIONS).  Failing this test means a relation was added to
/// EdgeRelation::ALL without going through an admission gate.
///
/// Note: the system-role path is a legitimate admission route per ADR-076 §D1
/// and §D3 — it is not a bypass.  It requires non-empty `adr`, `role`, and
/// `disposition` fields.  A `FailsEliminator` disposition additionally requires
/// a valid family code and a non-empty `kept_because` justification.
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
             exception in SYSTEM_ROLE_EXCEPTIONS (ADR-076 §D2). \
             To add a new relation via the certificate path: run its certificate \
             module to pass all seven eliminators and add it to CERTIFIED_RELATIONS. \
             To add via the system-role path (ADR-076 §D1/§D3): add a \
             SystemRoleException with non-empty `adr`, `role`, and a \
             `disposition` — either SurvivesAll or FailsEliminator {{ family, \
             kept_because }} for any eliminator the relation cannot defeat.",
        );
    }
}
