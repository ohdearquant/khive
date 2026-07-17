//! Formal concept-subtype endpoint rules over the closed relation set.

use khive_types::{EdgeEndpointRule, EdgeRelation, EndpointKind};

/// Shorthand macro — all formal-math subtypes belong to `kind = "concept"`.
macro_rules! formal_ep {
    ($et:literal) => {
        EndpointKind::EntityOfType {
            kind: "concept",
            entity_type: $et,
        }
    };
}

/// Additive edge endpoint rules for the formal-math ontology.
///
/// Relations covered: `depends_on` (14 rules), `instance_of` (1), `extends`
/// (2), `variant_of` (4). Total: 21 rules.
/// See `crates/khive-pack-formal/docs/api/formal-edge-rules.md`.
pub(crate) static FORMAL_EDGE_RULES: [EdgeEndpointRule; 21] = [
    // Dependency direction is consumer to prerequisite.
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: formal_ep!("theorem"),
        target: formal_ep!("theorem"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: formal_ep!("theorem"),
        target: formal_ep!("definition"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: formal_ep!("theorem"),
        target: formal_ep!("structure"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: formal_ep!("theorem"),
        target: formal_ep!("axiom"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: formal_ep!("definition"),
        target: formal_ep!("definition"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: formal_ep!("definition"),
        target: formal_ep!("structure"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: formal_ep!("definition"),
        target: formal_ep!("theorem"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: formal_ep!("definition"),
        target: formal_ep!("axiom"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: formal_ep!("instance"),
        target: formal_ep!("structure"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: formal_ep!("instance"),
        target: formal_ep!("definition"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: formal_ep!("goal"),
        target: formal_ep!("theorem"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: formal_ep!("goal"),
        target: formal_ep!("definition"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: formal_ep!("goal"),
        target: formal_ep!("structure"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: formal_ep!("goal"),
        target: formal_ep!("axiom"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::InstanceOf,
        source: formal_ep!("instance"),
        target: formal_ep!("structure"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Extends,
        source: formal_ep!("structure"),
        target: formal_ep!("structure"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Extends,
        source: formal_ep!("definition"),
        target: formal_ep!("definition"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::VariantOf,
        source: formal_ep!("theorem"),
        target: formal_ep!("theorem"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::VariantOf,
        source: formal_ep!("definition"),
        target: formal_ep!("definition"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::VariantOf,
        source: formal_ep!("goal"),
        target: formal_ep!("theorem"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::VariantOf,
        source: formal_ep!("goal"),
        target: formal_ep!("definition"),
    },
];
