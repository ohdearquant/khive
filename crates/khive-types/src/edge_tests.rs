use super::*;
use alloc::string::ToString;

#[test]
fn all_has_twenty_variants() {
    assert_eq!(EdgeRelation::ALL.len(), 20);
}

#[test]
fn all_ten_categories_covered() {
    let mut cats = alloc::vec::Vec::new();
    for r in EdgeRelation::ALL {
        let c = r.category();
        if !cats.contains(&c) {
            cats.push(c);
        }
    }
    assert_eq!(cats.len(), 10, "all 10 categories must be represented");
}

#[test]
fn owns_is_directional_ownership() {
    assert_eq!("owns".parse::<EdgeRelation>().unwrap(), EdgeRelation::Owns);
    assert_eq!("OWNS".parse::<EdgeRelation>().unwrap(), EdgeRelation::Owns);
    assert_eq!(EdgeRelation::Owns.to_string(), "owns");
    assert_eq!(EdgeRelation::Owns.category(), EdgeCategory::Ownership);
    assert!(!EdgeRelation::Owns.is_symmetric());
    assert_eq!(EdgeRelation::Owns.canonical_endpoints(9, 2), (9, 2));
}

#[test]
fn display_roundtrip_for_all() {
    for relation in EdgeRelation::ALL {
        let s = relation.to_string();
        let parsed: EdgeRelation = s.parse().expect("display output should re-parse");
        assert_eq!(parsed, relation);
    }
}

#[test]
fn from_str_case_insensitive() {
    assert_eq!(
        "Extends".parse::<EdgeRelation>().unwrap(),
        EdgeRelation::Extends
    );
    assert_eq!(
        "extends".parse::<EdgeRelation>().unwrap(),
        EdgeRelation::Extends
    );
    assert_eq!(
        "EXTENDS".parse::<EdgeRelation>().unwrap(),
        EdgeRelation::Extends
    );
}

#[test]
fn from_str_hyphen_tolerant() {
    assert_eq!(
        "part_of".parse::<EdgeRelation>().unwrap(),
        EdgeRelation::PartOf
    );
    assert_eq!(
        "part-of".parse::<EdgeRelation>().unwrap(),
        EdgeRelation::PartOf
    );
    assert_eq!(
        "partof".parse::<EdgeRelation>().unwrap(),
        EdgeRelation::PartOf
    );

    assert_eq!(
        "introduced_by".parse::<EdgeRelation>().unwrap(),
        EdgeRelation::IntroducedBy
    );
    assert_eq!(
        "introduced-by".parse::<EdgeRelation>().unwrap(),
        EdgeRelation::IntroducedBy
    );
}

#[test]
fn from_str_unknown_returns_error_with_list() {
    let err = "related_to".parse::<EdgeRelation>().unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("related_to"),
        "error should mention the bad input"
    );
    assert!(
        msg.contains("contains"),
        "error should list valid relations"
    );
    assert!(
        msg.contains("derived_from"),
        "error should list derived_from"
    );
    assert!(msg.contains("precedes"), "error should list precedes");
    assert!(msg.contains("annotates"), "error should list annotates");
}

#[test]
fn edge_relation_bang_rejected() {
    for bad in ["supports!", "part/of", "depends.on", "competes with"] {
        let err = bad
            .parse::<EdgeRelation>()
            .expect_err("malformed punctuation/whitespace must be rejected");
        assert_eq!(err.domain, "edge_relation");
        assert_eq!(err.value, bad);
    }
}

#[test]
fn category_returns_correct_group() {
    assert_eq!(EdgeRelation::Contains.category(), EdgeCategory::Structure);
    assert_eq!(EdgeRelation::PartOf.category(), EdgeCategory::Structure);
    assert_eq!(EdgeRelation::InstanceOf.category(), EdgeCategory::Structure);
    assert_eq!(EdgeRelation::LinksTo.category(), EdgeCategory::Structure);
    assert_eq!(EdgeRelation::LocatedIn.category(), EdgeCategory::Structure);

    assert_eq!(EdgeRelation::Extends.category(), EdgeCategory::Derivation);
    assert_eq!(EdgeRelation::VariantOf.category(), EdgeCategory::Derivation);
    assert_eq!(
        EdgeRelation::IntroducedBy.category(),
        EdgeCategory::Derivation
    );
    assert_eq!(
        EdgeRelation::Supersedes.category(),
        EdgeCategory::Derivation
    );

    assert_eq!(EdgeRelation::DependsOn.category(), EdgeCategory::Dependency);
    assert_eq!(EdgeRelation::Enables.category(), EdgeCategory::Dependency);

    assert_eq!(
        EdgeRelation::Implements.category(),
        EdgeCategory::Implementation
    );

    assert_eq!(
        EdgeRelation::DerivedFrom.category(),
        EdgeCategory::Provenance
    );
    assert_eq!(EdgeRelation::Precedes.category(), EdgeCategory::Temporal);

    assert_eq!(EdgeRelation::CompetesWith.category(), EdgeCategory::Lateral);
    assert_eq!(EdgeRelation::ComposedWith.category(), EdgeCategory::Lateral);

    assert_eq!(EdgeRelation::Annotates.category(), EdgeCategory::Annotation);
}

#[test]
fn from_str_new_relations() {
    assert_eq!(
        "derived_from".parse::<EdgeRelation>().unwrap(),
        EdgeRelation::DerivedFrom
    );
    assert_eq!(
        "derived-from".parse::<EdgeRelation>().unwrap(),
        EdgeRelation::DerivedFrom
    );
    assert_eq!(
        "derivedfrom".parse::<EdgeRelation>().unwrap(),
        EdgeRelation::DerivedFrom
    );
    assert_eq!(
        "precedes".parse::<EdgeRelation>().unwrap(),
        EdgeRelation::Precedes
    );
}

#[test]
fn from_str_links_to() {
    assert_eq!(
        "links_to".parse::<EdgeRelation>().unwrap(),
        EdgeRelation::LinksTo
    );
    assert_eq!(
        "links-to".parse::<EdgeRelation>().unwrap(),
        EdgeRelation::LinksTo
    );
    assert_eq!(
        "linksto".parse::<EdgeRelation>().unwrap(),
        EdgeRelation::LinksTo
    );
    assert_eq!(EdgeRelation::LinksTo.to_string(), "links_to");
    assert_eq!(EdgeRelation::LinksTo.category(), EdgeCategory::Structure);
}

#[test]
fn from_str_located_in() {
    assert_eq!(
        "located_in".parse::<EdgeRelation>().unwrap(),
        EdgeRelation::LocatedIn
    );
    assert_eq!(
        "located-in".parse::<EdgeRelation>().unwrap(),
        EdgeRelation::LocatedIn
    );
    assert_eq!(
        "locatedin".parse::<EdgeRelation>().unwrap(),
        EdgeRelation::LocatedIn
    );
    assert_eq!(EdgeRelation::LocatedIn.to_string(), "located_in");
    assert_eq!(EdgeRelation::LocatedIn.category(), EdgeCategory::Structure);
}

#[test]
fn is_symmetric_only_for_lateral_peer_relations() {
    assert!(EdgeRelation::CompetesWith.is_symmetric());
    assert!(EdgeRelation::ComposedWith.is_symmetric());
    assert!(!EdgeRelation::DependsOn.is_symmetric());
    assert!(!EdgeRelation::DerivedFrom.is_symmetric());
    assert!(!EdgeRelation::Precedes.is_symmetric());
    assert!(!EdgeRelation::Extends.is_symmetric());
    assert!(!EdgeRelation::LinksTo.is_symmetric());
    assert!(!EdgeRelation::LocatedIn.is_symmetric());
}

/// The symmetric relations, named here rather than derived from `is_symmetric`
/// so the tests below do not take their expectation from the code under test.
const SYMMETRIC: [EdgeRelation; 2] = [EdgeRelation::CompetesWith, EdgeRelation::ComposedWith];

/// Endpoint pairs with source less than, greater than and equal to target.
const PAIRS: [(u8, u8); 3] = [(1, 2), (2, 1), (7, 7)];

#[test]
fn canonical_endpoints_sorts_symmetric_relations() {
    for relation in SYMMETRIC {
        assert!(relation.is_symmetric(), "{relation}");
        for (source, target) in PAIRS {
            assert_eq!(
                relation.canonical_endpoints(source, target),
                (source.min(target), source.max(target)),
                "{relation}: {source} to {target}"
            );
        }
    }
}

#[test]
fn canonical_endpoints_leaves_other_relations_as_given() {
    let others: alloc::vec::Vec<EdgeRelation> = EdgeRelation::ALL
        .into_iter()
        .filter(|relation| !SYMMETRIC.contains(relation))
        .collect();
    assert_eq!(others.len(), EdgeRelation::ALL.len() - SYMMETRIC.len());
    for relation in others {
        assert!(!relation.is_symmetric(), "{relation}");
        for (source, target) in PAIRS {
            assert_eq!(
                relation.canonical_endpoints(source, target),
                (source, target),
                "{relation}: {source} to {target}"
            );
        }
    }
}

#[test]
fn from_str_epistemic_relations() {
    assert_eq!(
        "supports".parse::<EdgeRelation>().unwrap(),
        EdgeRelation::Supports
    );
    assert_eq!(
        "refutes".parse::<EdgeRelation>().unwrap(),
        EdgeRelation::Refutes
    );
    assert_eq!(
        "Supports".parse::<EdgeRelation>().unwrap(),
        EdgeRelation::Supports
    );
    assert_eq!(
        "REFUTES".parse::<EdgeRelation>().unwrap(),
        EdgeRelation::Refutes
    );
    assert_eq!(EdgeRelation::Supports.category(), EdgeCategory::Epistemic);
    assert_eq!(EdgeRelation::Refutes.category(), EdgeCategory::Epistemic);
    assert!(!EdgeRelation::Supports.is_symmetric());
    assert!(!EdgeRelation::Refutes.is_symmetric());
}

#[cfg(feature = "serde")]
#[test]
fn serde_snake_case_roundtrip() {
    let rel = EdgeRelation::IntroducedBy;
    let json = serde_json::to_string(&rel).unwrap();
    assert_eq!(json, "\"introduced_by\"");
    let parsed: EdgeRelation = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, rel);
}

#[cfg(feature = "serde")]
#[test]
fn serde_new_relations_roundtrip() {
    for rel in [EdgeRelation::DerivedFrom, EdgeRelation::Precedes] {
        let json = serde_json::to_string(&rel).unwrap();
        let parsed: EdgeRelation = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, rel);
    }
}

#[cfg(feature = "serde")]
#[test]
fn serde_epistemic_relations_roundtrip() {
    let sup_json = serde_json::to_string(&EdgeRelation::Supports).unwrap();
    assert_eq!(sup_json, "\"supports\"");
    let sup_parsed: EdgeRelation = serde_json::from_str(&sup_json).unwrap();
    assert_eq!(sup_parsed, EdgeRelation::Supports);

    let ref_json = serde_json::to_string(&EdgeRelation::Refutes).unwrap();
    assert_eq!(ref_json, "\"refutes\"");
    let ref_parsed: EdgeRelation = serde_json::from_str(&ref_json).unwrap();
    assert_eq!(ref_parsed, EdgeRelation::Refutes);
}
