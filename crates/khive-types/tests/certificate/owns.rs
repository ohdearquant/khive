//! ADR-197 ownership certificate over the six-node filings fixture.

use std::collections::BTreeSet;

use crate::harness::{
    all_pairs, assert_defeats_eliminator, assert_eliminated_by, EliminatorCheck, Fixture,
    GraphTriple,
};

const R: &str = "owns";
const GRAPH: &[GraphTriple] = &[
    ("Parent", "kind", "org"),
    ("Subsidiary", "kind", "org"),
    ("Target", "kind", "org"),
    ("IndexFund", "kind", "org"),
    ("Jane", "kind", "person"),
    ("John", "kind", "person"),
    ("Subsidiary", "part_of", "Parent"),
    ("Parent", R, "Subsidiary"),
    ("Parent", "attr:pct:80", "Subsidiary"),
    ("IndexFund", R, "Subsidiary"),
    ("IndexFund", "attr:pct:6", "Subsidiary"),
    ("IndexFund", R, "Target"),
    ("IndexFund", "attr:pct:7", "Target"),
    ("Jane", R, "Target"),
    ("Jane", "attr:pct:0.5", "Target"),
    ("Jane", "part_of", "Target"),
    ("John", "part_of", "Target"),
];

fn kind_of(graph: &[GraphTriple], node: &str) -> Option<&'static str> {
    graph
        .iter()
        .find(|(source, relation, _)| *source == node && *relation == "kind")
        .map(|(_, _, kind)| *kind)
}

fn incoming(graph: &[GraphTriple], target: &str, relation: &str) -> BTreeSet<&'static str> {
    graph
        .iter()
        .filter(|(_, rel, end)| *rel == relation && *end == target)
        .map(|(source, _, _)| *source)
        .collect()
}

fn outgoing(graph: &[GraphTriple], source: &str, relation: &str) -> BTreeSet<&'static str> {
    graph
        .iter()
        .filter(|(start, rel, _)| *start == source && *rel == relation)
        .map(|(_, _, target)| *target)
        .collect()
}

fn compare<T: PartialEq + std::fmt::Debug>(cheaper: T, original: T) -> EliminatorCheck {
    if cheaper == original {
        EliminatorCheck::Eliminated {
            shared_answer: format!("{original:?}"),
            reason: "the transformed graph gives the same answer".into(),
        }
    } else {
        EliminatorCheck::Passes {
            cheaper: format!("{cheaper:?}"),
            r_answer: format!("{original:?}"),
        }
    }
}

fn part_of_encoding(graph: &[GraphTriple], marker: Option<&'static str>) -> Vec<GraphTriple> {
    let mut encoded = BTreeSet::new();
    for &(source, relation, target) in graph {
        if relation == R {
            encoded.insert((source, "part_of", target));
            if let Some(marker) = marker {
                encoded.insert((source, marker, target));
            }
        } else {
            encoded.insert((source, relation, target));
        }
    }
    encoded.into_iter().collect()
}

fn incoming_parts(graph: &[GraphTriple], root: &'static str) -> BTreeSet<&'static str> {
    let mut visited = BTreeSet::from([root]);
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        for source in incoming(graph, node, "part_of") {
            if visited.insert(source) {
                pending.push(source);
            }
        }
    }
    visited.remove(root);
    visited
}

fn cv_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let mut encoded = graph
        .iter()
        .copied()
        .filter(|(_, relation, _)| *relation != R)
        .collect::<Vec<_>>();
    let mut refused = BTreeSet::new();
    for &(owner, relation, owned) in graph {
        if relation != R {
            continue;
        }
        // The fixture's effective base-plus-KG contract permits Org contains Org,
        // but never Org contains Person. The live KG test pins these two cases.
        if kind_of(graph, owned) == Some("org") && kind_of(graph, owner) == Some("org") {
            encoded.push((owned, "contains", owner));
        } else {
            refused.insert((owner, owned));
        }
    }
    compare(
        (refused, outgoing(&encoded, "Target", "contains")),
        (BTreeSet::new(), outgoing(graph, "Target", "contains")),
    )
}

fn er_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let encoded = part_of_encoding(graph, None);
    let holders = incoming(&encoded, "Target", "part_of")
        .into_iter()
        .filter(|node| matches!(kind_of(graph, node), Some("person" | "org")))
        .collect::<BTreeSet<_>>();
    compare(holders, incoming(graph, "Target", R))
}

fn at_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let encoded = part_of_encoding(graph, Some("attr:predicate:beneficial_owner"));
    compare(
        incoming_parts(&encoded, "Parent"),
        incoming_parts(graph, "Parent"),
    )
}

fn po_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let encoded = part_of_encoding(graph, Some("attr:sign:holding"));
    compare(
        incoming(&encoded, "Target", "part_of"),
        incoming(graph, "Target", "part_of"),
    )
}

fn existing_reachability(graph: &[GraphTriple], root: &'static str) -> BTreeSet<&'static str> {
    let mut visited = BTreeSet::from([root]);
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        for &(source, relation, target) in graph {
            if source == node
                && relation != R
                && relation != "kind"
                && !relation.starts_with("attr:")
                && visited.insert(target)
            {
                pending.push(target);
            }
        }
    }
    visited.remove(root);
    visited
}

fn ch_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    compare(
        existing_reachability(graph, "IndexFund"),
        outgoing(graph, "IndexFund", R),
    )
}

fn mv_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let view = existing_reachability(graph, "IndexFund");
    compare(view, outgoing(graph, "IndexFund", R))
}

fn sr_check(graph: &'static [GraphTriple]) -> EliminatorCheck {
    let encoded = part_of_encoding(graph, Some("attr:subrelation:owns"));
    compare(
        incoming_parts(&encoded, "Parent"),
        incoming_parts(graph, "Parent"),
    )
}

pub const FIXTURES: &[Fixture] = &[
    Fixture {
        eliminator: "Cv",
        cheaper_encoding: "converse of contains",
        graph: GRAPH,
        query: "what is in Target's organizational tree, and which writes were refused?",
        check: cv_check,
    },
    Fixture {
        eliminator: "Er",
        cheaper_encoding: "part_of restricted to person/org -> org",
        graph: GRAPH,
        query: "who holds Target?",
        check: er_check,
    },
    Fixture {
        eliminator: "At",
        cheaper_encoding: "part_of with a beneficial_owner predicate attribute",
        graph: GRAPH,
        query: "what are the transitive parts of Parent?",
        check: at_check,
    },
    Fixture {
        eliminator: "Po",
        cheaper_encoding: "part_of with a holding sign attribute",
        graph: GRAPH,
        query: "who are the members of Target, without interpreting sign?",
        check: po_check,
    },
    Fixture {
        eliminator: "Ch",
        cheaper_encoding: "a chain of existing relations",
        graph: GRAPH,
        query: "what does IndexFund hold?",
        check: ch_check,
    },
    Fixture {
        eliminator: "Mv",
        cheaper_encoding: "a reachability view over existing relations",
        graph: GRAPH,
        query: "what does IndexFund hold?",
        check: mv_check,
    },
    Fixture {
        eliminator: "Sr",
        cheaper_encoding: "a typed sub-relation of part_of",
        graph: GRAPH,
        query: "what does a parent-relation reader see as parts of Parent?",
        check: sr_check,
    },
];

#[test]
fn owns_defeats_all_seven_cheaper_encodings() {
    for fixture in FIXTURES {
        assert_defeats_eliminator(R, fixture);
    }
}

#[test]
fn each_owns_check_rejects_a_nonempty_redundant_control() {
    const CV: &[GraphTriple] = &[
        ("Target", "kind", "org"),
        ("IndexFund", "kind", "org"),
        ("IndexFund", R, "Target"),
        ("Target", "contains", "IndexFund"),
    ];
    const ER: &[GraphTriple] = &[
        ("Target", "kind", "org"),
        ("Jane", "kind", "person"),
        ("Jane", R, "Target"),
    ];
    const PARTS: &[GraphTriple] = &[
        ("Subsidiary", "part_of", "Parent"),
        ("Subsidiary", R, "Parent"),
    ];
    const PO: &[GraphTriple] = &[("Jane", "part_of", "Target"), ("Jane", R, "Target")];
    const REACHABLE: &[GraphTriple] = &[
        ("IndexFund", "part_of", "Subsidiary"),
        ("IndexFund", "part_of", "Target"),
        ("IndexFund", R, "Subsidiary"),
        ("IndexFund", R, "Target"),
    ];
    for (fixture, graph) in FIXTURES
        .iter()
        .zip([CV, ER, PARTS, PO, REACHABLE, REACHABLE, PARTS])
    {
        assert!(!all_pairs(graph, R).is_empty());
        assert_eliminated_by(R, &Fixture { graph, ..*fixture });
    }
}

#[test]
fn filings_fixture_preserves_the_stake_membership_distinction() {
    for node in ["Parent", "Subsidiary", "Target", "IndexFund"] {
        assert_eq!(kind_of(GRAPH, node), Some("org"));
    }
    for node in ["Jane", "John"] {
        assert_eq!(kind_of(GRAPH, node), Some("person"));
    }
    assert_eq!(all_pairs(GRAPH, R).len(), 4);
    assert_eq!(all_pairs(GRAPH, "part_of").len(), 3);
    assert_eq!(
        incoming(GRAPH, "Target", R),
        BTreeSet::from(["IndexFund", "Jane"])
    );
    assert_eq!(
        incoming(GRAPH, "Target", "part_of"),
        BTreeSet::from(["Jane", "John"])
    );
    assert!(existing_reachability(GRAPH, "IndexFund").is_empty());
    assert!(!GRAPH.iter().any(|(source, relation, target)| {
        (*source == "IndexFund" || *target == "IndexFund")
            && *relation != R
            && *relation != "kind"
            && !relation.starts_with("attr:")
    }));
    assert!(GRAPH.contains(&("Parent", R, "Subsidiary")));
    assert!(GRAPH.contains(&("Subsidiary", "part_of", "Parent")));
    let encoded = part_of_encoding(GRAPH, Some("attr:predicate:beneficial_owner"));
    assert_eq!(
        encoded
            .iter()
            .filter(|triple| **triple == ("Jane", "part_of", "Target"))
            .count(),
        1,
        "two independent facts collapse to one part_of natural key"
    );
    assert_eq!(
        incoming_parts(GRAPH, "Parent"),
        BTreeSet::from(["Subsidiary"])
    );
    assert_eq!(
        incoming_parts(&encoded, "Parent"),
        BTreeSet::from(["IndexFund", "Subsidiary"])
    );
}
