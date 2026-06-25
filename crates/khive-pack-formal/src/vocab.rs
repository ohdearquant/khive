//! Formal-math pack vocabulary: additive edge endpoint rules over the closed
//! relation set, keyed on `entity_type` via `EntityOfType`.
//!
//! These rules extend the base contract without tightening it. Every endpoint
//! uses `EndpointKind::EntityOfType` so the match enforces the full
//! `(EntityKind, entity_type)` registry pair required by ADR-001:102: the base
//! `kind` must be `"concept"` for all six formal-math subtypes, and the
//! `entity_type` must match the declared subtype. The closed `EdgeRelation`
//! variants are unchanged.

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
pub(crate) static FORMAL_EDGE_RULES: [EdgeEndpointRule; 21] = [
    // ── depends_on: prerequisite chain (source uses / builds on target) ────
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
    // ── instance_of: instance implements a structure ───────────────────────
    EdgeEndpointRule {
        relation: EdgeRelation::InstanceOf,
        source: formal_ep!("instance"),
        target: formal_ep!("structure"),
    },
    // ── extends: structural / definitional inheritance ─────────────────────
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
    // ── variant_of: restatement / anti-farm signal ────────────────────────
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
