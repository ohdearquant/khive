//! KgPack struct, `Pack` trait impl, and pack-extensible edge endpoint rules.

use khive_types::{EdgeEndpointRule, EdgeRelation, EndpointKind, HandlerDef, Pack};

use crate::handler_defs::KG_HANDLERS;

/// Pack-extensible edge endpoint rules — adds person→org and org→org pairs to the base allowlist.
pub(crate) static KG_EDGE_RULES: [EdgeEndpointRule; 7] = [
    EdgeEndpointRule {
        relation: EdgeRelation::PartOf,
        source: EndpointKind::EntityOfKind("person"),
        target: EndpointKind::EntityOfKind("org"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::InstanceOf,
        source: EndpointKind::EntityOfKind("person"),
        target: EndpointKind::EntityOfKind("org"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::EntityOfKind("org"),
        target: EndpointKind::EntityOfKind("org"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Enables,
        source: EndpointKind::EntityOfKind("org"),
        target: EndpointKind::EntityOfKind("org"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: EndpointKind::EntityOfKind("org"),
        target: EndpointKind::EntityOfKind("org"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::PartOf,
        source: EndpointKind::EntityOfKind("org"),
        target: EndpointKind::EntityOfKind("org"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Precedes,
        source: EndpointKind::EntityOfKind("org"),
        target: EndpointKind::EntityOfKind("org"),
    },
];

/// KG pack vocabulary declaration.
pub struct KgPack {
    pub(crate) runtime: khive_runtime::KhiveRuntime,
}

impl Pack for KgPack {
    const NAME: &'static str = "kg";
    const NOTE_KINDS: &'static [&'static str] = &[
        "observation",
        "insight",
        "question",
        "decision",
        "reference",
    ];
    const ENTITY_KINDS: &'static [&'static str] = &[
        "concept", "document", "dataset", "project", "person", "org", "artifact", "service",
        "resource",
    ];
    const HANDLERS: &'static [HandlerDef] = &KG_HANDLERS;
    const EDGE_RULES: &'static [EdgeEndpointRule] = &KG_EDGE_RULES;
}

impl KgPack {
    /// Create a new KG pack backed by the given runtime.
    pub fn new(runtime: khive_runtime::KhiveRuntime) -> Self {
        Self { runtime }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_types::EndpointKind;

    /// Extract a comparable (source_str, target_str) pair from an EndpointKind.
    fn endpoint_str(ep: &EndpointKind) -> &'static str {
        match ep {
            EndpointKind::EntityOfKind(k) => k,
            EndpointKind::NoteOfKind(k) => k,
            EndpointKind::EntityOfType { kind, .. } => kind,
        }
    }

    /// ADR-076 §D2: no duplicate (relation, source, target) triples in KG_EDGE_RULES.
    ///
    /// A duplicated triple would be a no-op additive rule (adding the same endpoint
    /// pair a second time changes nothing) and is a sign of a copy-paste error.
    /// Semantic similarity between relations (e.g. multiple relations accepting
    /// `org→org`) is expected and correct; this test checks only for exact-triple
    /// duplicates, not for shared per-relation endpoint sets.
    #[test]
    fn kg_pack_edge_rules_contain_no_duplicate_triples() {
        let triples: Vec<(&str, &str, &str)> = KG_EDGE_RULES
            .iter()
            .map(|r| {
                (
                    r.relation.as_str(),
                    endpoint_str(&r.source),
                    endpoint_str(&r.target),
                )
            })
            .collect();

        let mut seen: Vec<(&str, &str, &str)> = triples.clone();
        seen.sort_unstable();

        let mut duplicates: Vec<(&str, &str, &str)> = vec![];
        for i in 1..seen.len() {
            if seen[i] == seen[i - 1] {
                duplicates.push(seen[i]);
            }
        }

        assert!(
            duplicates.is_empty(),
            "KG_EDGE_RULES contains duplicate triples: {duplicates:?}; \
             remove the redundant entries"
        );
    }

    /// Regression: the KG_EDGE_RULES array has exactly the expected relations.
    ///
    /// A change to the set of relations that get pack-level endpoint extensions
    /// should be a deliberate, reviewed decision — not an accidental side effect.
    #[test]
    fn kg_pack_edge_rules_cover_expected_relations() {
        let mut seen: Vec<&str> = KG_EDGE_RULES.iter().map(|r| r.relation.as_str()).collect();
        seen.sort_unstable();
        seen.dedup();

        let expected = &[
            "contains",
            "depends_on",
            "enables",
            "instance_of",
            "part_of",
            "precedes",
        ];

        assert_eq!(
            seen, *expected,
            "KG_EDGE_RULES covers a different relation set than expected; \
             update this test if the change is intentional"
        );
    }
}
