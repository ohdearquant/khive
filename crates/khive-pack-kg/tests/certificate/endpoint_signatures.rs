//! Endpoint-signature distinguishability audit — supplementary Er tripwire.
//!
//! This module is a SUPPLEMENTARY redundancy signal, not the Er-eliminator
//! arbiter.  ADR-076 §D2's Er eliminator is defeated by a concrete fixture (a
//! small graph + a query where the endpoint-restricted cheaper encoding gives a
//! wrong answer).  Identical endpoint signatures are a SIGNAL that Er analysis
//! is warranted — they are not proof of redundancy, because two relations may
//! share a signature yet diverge in semantics for reasons a fixture can expose.
//! `contains`/`part_of` (ADR-076 §D4) are the canonical example of distinct
//! relations that happen to share endpoint pairs in some domains.
//!
//! When this test flags a collision, the resolution is one of:
//!   1. A passing Er fixture in the relation's certificate entry
//!      (CERTIFIED_RELATIONS in coverage.rs), or
//!   2. A declared system-role exception in SYSTEM_ROLE_EXCEPTIONS (coverage.rs)
//!      with a `FailsEliminator` disposition naming the justification.
//! Do NOT resolve a new collision by appending to D3_RATIFIED_COLLISIONS here —
//! that list is closed to the ADR-076 §D3 ratified case only.
//!
//! Known ratified collision (ADR-076 §D3): `supports` and `refutes` share an
//! identical base endpoint signature.  They are kept by declared system role
//! (ADR-055).  This test asserts theirs is the ONLY ratified collision.

use khive_pack_kg::KgPack;
use khive_runtime::operations::base_entity_endpoint_rules;
use khive_types::{EndpointKind, Pack};

/// Extract the kind string from an EndpointKind for signature comparison.
fn endpoint_str(ep: &EndpointKind) -> &'static str {
    match ep {
        EndpointKind::EntityOfKind(k) => k,
        EndpointKind::NoteOfKind(k) => k,
        EndpointKind::EntityOfType { kind, .. } => kind,
    }
}

/// Collect sorted (source, target) pairs grouped by relation name.
fn signatures(triples: &[(String, String, String)]) -> Vec<(String, Vec<(String, String)>)> {
    let mut rels: Vec<String> = triples.iter().map(|(_, r, _)| r.clone()).collect();
    rels.sort_unstable();
    rels.dedup();
    rels.into_iter()
        .map(|rel| {
            let mut pairs: Vec<(String, String)> = triples
                .iter()
                .filter(|(_, r, _)| *r == rel)
                .map(|(src, _, tgt)| (src.clone(), tgt.clone()))
                .collect();
            pairs.sort_unstable();
            (rel, pairs)
        })
        .collect()
}

/// Signature collisions that are ratified by ADR-076 §D3 and require no further action.
///
/// This list is CLOSED.  It contains exactly the collisions that ADR-076 §D3
/// explicitly records as "kept by system role despite failing the Po eliminator."
/// A new collision discovered by the test below must NOT be resolved by appending
/// here — resolve it via the certificate (CERTIFIED_RELATIONS) or a
/// SYSTEM_ROLE_EXCEPTIONS entry with a FailsEliminator disposition (coverage.rs).
const D3_RATIFIED_COLLISIONS: &[(&str, &str)] = &[("refutes", "supports")];

/// Endpoint-signature collision tripwire (supplementary Er signal, ADR-076 §D2).
///
/// Flags any pair of distinct relations that share an identical (source, target)
/// endpoint-pair set.  A collision is a redundancy SIGNAL that requires Er
/// analysis — it is not proof of redundancy.  Resolve a new collision via the
/// certificate admission path or a system-role exception in coverage.rs, not by
/// expanding D3_RATIFIED_COLLISIONS.
///
/// Uses the real live rules from khive-runtime and khive-pack-kg — not copies.
#[test]
fn base_and_pack_endpoint_signatures_are_pairwise_distinct_except_d3_ratified_collisions() {
    // Collect live base entity endpoint rules from khive-runtime.
    let mut all_triples: Vec<(String, String, String)> = base_entity_endpoint_rules()
        .iter()
        .map(|(src, rel, tgt)| (src.to_string(), rel.as_str().to_string(), tgt.to_string()))
        .collect();

    // Collect live KG pack additive rules via Pack::EDGE_RULES.
    for rule in <KgPack as Pack>::EDGE_RULES {
        all_triples.push((
            endpoint_str(&rule.source).to_string(),
            rule.relation.as_str().to_string(),
            endpoint_str(&rule.target).to_string(),
        ));
    }

    let sigs = signatures(&all_triples);

    let mut collisions: Vec<(String, String)> = vec![];
    for i in 0..sigs.len() {
        for j in (i + 1)..sigs.len() {
            let (rel_a, pairs_a) = &sigs[i];
            let (rel_b, pairs_b) = &sigs[j];
            if pairs_a == pairs_b {
                let mut pair = [rel_a.as_str(), rel_b.as_str()];
                pair.sort_unstable();
                let normalised = (pair[0], pair[1]);
                if !D3_RATIFIED_COLLISIONS.contains(&normalised) {
                    collisions.push((rel_a.clone(), rel_b.clone()));
                }
            }
        }
    }

    assert!(
        collisions.is_empty(),
        "endpoint-signature collision(s) detected — two distinct relations share an \
         identical (source, target) pair set, an Er-eliminator signal per ADR-076 §D2. \
         This test is a supplementary tripwire; resolve each collision via the \
         ADR-076 admission paths: add a passing Er fixture to CERTIFIED_RELATIONS, or \
         add a SystemRoleException with a FailsEliminator disposition in coverage.rs. \
         Do NOT append to D3_RATIFIED_COLLISIONS — that list is closed to the \
         ADR-076 §D3 ratified case only.\n{collisions:#?}"
    );
}

/// Guard: `D3_RATIFIED_COLLISIONS` is a closed list containing exactly the one ratified case.
///
/// Any append to this list to suppress a new collision is mechanically blocked — this
/// equality assertion must be updated, which forces reviewer attention.  Resolve new
/// collisions via the certificate admission path (CERTIFIED_RELATIONS in coverage.rs) or
/// a `FailsEliminator` disposition in SYSTEM_ROLE_EXCEPTIONS — not by expanding this list.
#[test]
fn d3_ratified_collisions_is_exactly_refutes_supports() {
    assert_eq!(
        D3_RATIFIED_COLLISIONS,
        &[("refutes", "supports")],
        "D3_RATIFIED_COLLISIONS must contain exactly the one ADR-076 §D3 ratified case \
         (\"refutes\", \"supports\"); silencing a new collision by appending here bypasses \
         the Er-eliminator analysis gate — resolve via the certificate admission path or a \
         FailsEliminator disposition in SYSTEM_ROLE_EXCEPTIONS instead"
    );
}

/// The `supports`/`refutes` identical-signature collision is present and intentional.
///
/// Documents the ADR-076 §D3 finding: `supports` and `refutes` share a base
/// endpoint signature yet are kept by declared system role (ADR-055).  This test
/// asserts the collision is real — any future rule change that accidentally
/// eliminates the shared signature gets a failing snapshot to investigate.
#[test]
fn supports_refutes_have_identical_base_signatures_adr076_d3() {
    let base_triples: Vec<(String, String, String)> = base_entity_endpoint_rules()
        .iter()
        .map(|(src, rel, tgt)| (src.to_string(), rel.as_str().to_string(), tgt.to_string()))
        .collect();

    let base_sigs = signatures(&base_triples);

    let supports_pairs = base_sigs
        .iter()
        .find(|(r, _)| r == "supports")
        .map(|(_, p)| p.clone())
        .expect("supports must appear in base rules");

    let refutes_pairs = base_sigs
        .iter()
        .find(|(r, _)| r == "refutes")
        .map(|(_, p)| p.clone())
        .expect("refutes must appear in base rules");

    assert_eq!(
        supports_pairs, refutes_pairs,
        "supports and refutes are expected to share an identical endpoint signature \
         per ADR-076 §D3; if this fails, revisit the D3 system-role declaration"
    );
}
