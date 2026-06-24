//! Formal-math pack vocabulary: additive edge endpoint rules over the closed
//! relation set, keyed on `entity_type` via `EntityOfType`.
//!
//! These rules extend the base contract without tightening it. Every endpoint
//! uses `EndpointKind::EntityOfType` so the match runs against
//! `Entity::entity_type`, not the base `kind` field (`"concept"` for all six
//! formal-math subtypes). The closed `EdgeRelation` variants are unchanged.

use khive_types::{EdgeEndpointRule, EdgeRelation, EndpointKind};

/// Additive edge endpoint rules for the formal-math ontology.
///
/// Relations covered: `depends_on` (14 rules), `instance_of` (1), `extends`
/// (2), `variant_of` (4). Total: 21 rules.
pub(crate) static FORMAL_EDGE_RULES: [EdgeEndpointRule; 21] = [
    // ── depends_on: prerequisite chain (source uses / builds on target) ────
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::EntityOfType("theorem"),
        target: EndpointKind::EntityOfType("theorem"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::EntityOfType("theorem"),
        target: EndpointKind::EntityOfType("definition"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::EntityOfType("theorem"),
        target: EndpointKind::EntityOfType("structure"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::EntityOfType("theorem"),
        target: EndpointKind::EntityOfType("axiom"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::EntityOfType("definition"),
        target: EndpointKind::EntityOfType("definition"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::EntityOfType("definition"),
        target: EndpointKind::EntityOfType("structure"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::EntityOfType("definition"),
        target: EndpointKind::EntityOfType("theorem"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::EntityOfType("definition"),
        target: EndpointKind::EntityOfType("axiom"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::EntityOfType("instance"),
        target: EndpointKind::EntityOfType("structure"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::EntityOfType("instance"),
        target: EndpointKind::EntityOfType("definition"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::EntityOfType("goal"),
        target: EndpointKind::EntityOfType("theorem"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::EntityOfType("goal"),
        target: EndpointKind::EntityOfType("definition"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::EntityOfType("goal"),
        target: EndpointKind::EntityOfType("structure"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::EntityOfType("goal"),
        target: EndpointKind::EntityOfType("axiom"),
    },
    // ── instance_of: instance implements a structure ───────────────────────
    EdgeEndpointRule {
        relation: EdgeRelation::InstanceOf,
        source: EndpointKind::EntityOfType("instance"),
        target: EndpointKind::EntityOfType("structure"),
    },
    // ── extends: structural / definitional inheritance ─────────────────────
    EdgeEndpointRule {
        relation: EdgeRelation::Extends,
        source: EndpointKind::EntityOfType("structure"),
        target: EndpointKind::EntityOfType("structure"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Extends,
        source: EndpointKind::EntityOfType("definition"),
        target: EndpointKind::EntityOfType("definition"),
    },
    // ── variant_of: restatement / anti-farm signal ────────────────────────
    EdgeEndpointRule {
        relation: EdgeRelation::VariantOf,
        source: EndpointKind::EntityOfType("theorem"),
        target: EndpointKind::EntityOfType("theorem"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::VariantOf,
        source: EndpointKind::EntityOfType("definition"),
        target: EndpointKind::EntityOfType("definition"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::VariantOf,
        source: EndpointKind::EntityOfType("goal"),
        target: EndpointKind::EntityOfType("theorem"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::VariantOf,
        source: EndpointKind::EntityOfType("goal"),
        target: EndpointKind::EntityOfType("definition"),
    },
];
