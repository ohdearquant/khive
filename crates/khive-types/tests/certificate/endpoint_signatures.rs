//! Endpoint-signature distinguishability audit for the closed relation set.
//!
//! ADR-076 §D2 requires that two distinct relations not share an identical
//! endpoint-pair signature. A shared signature means the relations are
//! indistinguishable by the query planner when choosing edges by kind alone,
//! which is a strong signal that one is redundant under the Er eliminator.
//!
//! This module audits the base endpoint contract declared in
//! `khive-runtime::operations` (inlined as a snapshot) and the KG-pack
//! additive rules (`KG_EDGE_RULES` in `khive-pack-kg::pack`).
//!
//! Known exception (ADR-076 §D3): `supports` and `refutes` share an identical
//! base endpoint signature. They are kept by declared system role (ADR-055),
//! not by the certificate. The test asserts this is the ONLY exception.

/// A base-rule triple: (source_kind, relation_name, target_kind).
/// `"*"` as source means "any entity kind" (used only by `instance_of`).
///
/// This is a snapshot of `const RULES` in `khive-runtime::operations::base_entity_rule_allows`.
/// If that private const changes, update this snapshot and re-validate the audit conclusion.
const BASE_RULES: &[(&str, &str, &str)] = &[
    // Structure
    ("concept", "contains", "concept"),
    ("project", "contains", "project"),
    ("project", "contains", "artifact"),
    ("org", "contains", "project"),
    ("org", "contains", "service"),
    ("concept", "part_of", "concept"),
    ("project", "part_of", "project"),
    ("project", "part_of", "org"),
    ("*", "instance_of", "concept"),
    ("service", "instance_of", "project"),
    // Derivation
    ("concept", "extends", "concept"),
    ("concept", "variant_of", "concept"),
    ("artifact", "variant_of", "artifact"),
    ("concept", "introduced_by", "document"),
    ("concept", "introduced_by", "person"),
    ("artifact", "introduced_by", "document"),
    // Provenance
    ("artifact", "derived_from", "dataset"),
    ("artifact", "derived_from", "document"),
    ("artifact", "derived_from", "project"),
    ("artifact", "derived_from", "artifact"),
    // Temporal
    ("document", "precedes", "document"),
    ("dataset", "precedes", "dataset"),
    ("artifact", "precedes", "artifact"),
    ("service", "precedes", "service"),
    ("project", "precedes", "project"),
    // Dependency
    ("project", "depends_on", "project"),
    ("service", "depends_on", "project"),
    ("service", "depends_on", "service"),
    ("service", "depends_on", "artifact"),
    ("service", "depends_on", "dataset"),
    ("artifact", "depends_on", "project"),
    ("artifact", "depends_on", "service"),
    ("concept", "enables", "concept"),
    ("service", "enables", "concept"),
    ("dataset", "enables", "concept"),
    // Implementation
    ("project", "implements", "concept"),
    ("service", "implements", "concept"),
    // Lateral
    ("concept", "competes_with", "concept"),
    ("project", "competes_with", "project"),
    ("service", "competes_with", "service"),
    ("concept", "composed_with", "concept"),
    ("project", "composed_with", "project"),
    // Versioning
    ("concept", "supersedes", "concept"),
    ("document", "supersedes", "document"),
    ("artifact", "supersedes", "artifact"),
    ("service", "supersedes", "service"),
    ("dataset", "supersedes", "dataset"),
    // Epistemic — identical signatures for supports/refutes (ADR-076 §D3 known exception)
    ("concept", "supports", "concept"),
    ("document", "supports", "concept"),
    ("dataset", "supports", "concept"),
    ("artifact", "supports", "concept"),
    ("concept", "refutes", "concept"),
    ("document", "refutes", "concept"),
    ("dataset", "refutes", "concept"),
    ("artifact", "refutes", "concept"),
];

/// KG-pack additive endpoint rules (snapshot of `KG_EDGE_RULES` in
/// `khive-pack-kg::pack`). Tuple form: (source_kind, relation_name, target_kind).
const PACK_RULES: &[(&str, &str, &str)] = &[
    ("person", "part_of", "org"),
    ("person", "instance_of", "org"),
    ("org", "depends_on", "org"),
    ("org", "enables", "org"),
    ("org", "contains", "org"),
    ("org", "part_of", "org"),
    ("org", "precedes", "org"),
];

/// Collect sorted `(source, target)` pairs per relation from a rule set.
///
/// Returns owned Strings to avoid lifetime-tying the result to a local slice.
fn signatures_from(rules: &[(&str, &str, &str)]) -> Vec<(String, Vec<(String, String)>)> {
    let mut relations: Vec<&str> = rules.iter().map(|(_, rel, _)| *rel).collect();
    relations.sort_unstable();
    relations.dedup();

    relations
        .into_iter()
        .map(|rel| {
            let mut pairs: Vec<(String, String)> = rules
                .iter()
                .filter(|(_, r, _)| *r == rel)
                .map(|(src, _, tgt)| (src.to_string(), tgt.to_string()))
                .collect();
            pairs.sort_unstable();
            (rel.to_string(), pairs)
        })
        .collect()
}

/// Identical endpoint signatures are a redundancy signal (ADR-076 §D2 Er eliminator).
///
/// The ONLY permitted exception is the `supports`/`refutes` pair, which is kept
/// by declared system role (ADR-055) per ADR-076 §D3. Any other pair of relations
/// sharing a signature is an unaudited collision and must be resolved before new
/// relations ship.
#[test]
fn base_and_pack_endpoint_signatures_are_pairwise_distinct_except_known_exceptions() {
    let mut all_rules: Vec<(&str, &str, &str)> = BASE_RULES.to_vec();
    all_rules.extend_from_slice(PACK_RULES);
    let sigs = signatures_from(&all_rules);

    /// Pairs whose identical signature is ratified by ADR-076 §D3.
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
        "endpoint-signature collision(s) detected — two distinct relations share \
         an identical (source, target) pair set, which is a redundancy signal \
         under ADR-076 §D2 Er eliminator. Resolve each or add a ratified \
         system-role exception to KNOWN_EXCEPTIONS:\n{collisions:#?}"
    );
}

/// The `supports`/`refutes` identical-signature exception is present and intentional.
///
/// Documents the ADR-076 §D3 finding: `supports` and `refutes` share a base
/// endpoint signature yet are kept by declared system role (ADR-055). This test
/// asserts the exception is real so that any future rule change that accidentally
/// eliminates the shared signature gets a failing snapshot to investigate.
#[test]
fn supports_refutes_have_identical_base_signatures_adr076_d3() {
    let base_sigs = signatures_from(BASE_RULES);

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
