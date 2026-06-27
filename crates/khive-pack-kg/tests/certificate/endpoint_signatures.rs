//! Endpoint-signature distinguishability audit using live rules.
//!
//! ADR-076 §D2 requires that two distinct relations not share an identical
//! endpoint-pair signature. This module builds signatures from the ACTUAL live
//! base endpoint contract (`khive_runtime::operations::base_entity_endpoint_rules`)
//! and the ACTUAL live KG pack rules (`<KgPack as Pack>::EDGE_RULES`). If either
//! changes, this test immediately reflects the current state — no manual snapshot
//! to keep in sync.
//!
//! Known exception (ADR-076 §D3): `supports` and `refutes` share an identical
//! base endpoint signature. They are kept by declared system role (ADR-055),
//! not by the certificate. The tests assert this is the ONLY exception.

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

/// Identical endpoint signatures are a redundancy signal (ADR-076 §D2 Er eliminator).
///
/// The ONLY permitted exception is the `supports`/`refutes` pair, which is kept
/// by declared system role (ADR-055) per ADR-076 §D3. Any other pair of relations
/// sharing a signature is an unaudited collision that must be resolved before new
/// relations ship.
///
/// Uses the real live rules from khive-runtime and khive-pack-kg — not copies.
#[test]
fn base_and_pack_endpoint_signatures_are_pairwise_distinct_except_known_exceptions() {
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

    // Pairs whose identical signature is ratified by ADR-076 §D3.
    const KNOWN_EXCEPTIONS: &[(&str, &str)] = &[("refutes", "supports")];

    let mut collisions: Vec<(String, String)> = vec![];
    for i in 0..sigs.len() {
        for j in (i + 1)..sigs.len() {
            let (rel_a, pairs_a) = &sigs[i];
            let (rel_b, pairs_b) = &sigs[j];
            if pairs_a == pairs_b {
                let mut pair = [rel_a.as_str(), rel_b.as_str()];
                pair.sort_unstable();
                let normalised = (pair[0], pair[1]);
                if !KNOWN_EXCEPTIONS.contains(&normalised) {
                    collisions.push((rel_a.clone(), rel_b.clone()));
                }
            }
        }
    }

    assert!(
        collisions.is_empty(),
        "endpoint-signature collision(s) detected — two distinct relations share an \
         identical (source, target) pair set, a redundancy signal under ADR-076 §D2 \
         Er eliminator. Resolve each or add a ratified system-role exception to \
         KNOWN_EXCEPTIONS:\n{collisions:#?}"
    );
}

/// The `supports`/`refutes` identical-signature exception is present and intentional.
///
/// Documents the ADR-076 §D3 finding: `supports` and `refutes` share a base
/// endpoint signature yet are kept by declared system role (ADR-055). This test
/// asserts the exception is real — any future rule change that accidentally
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
