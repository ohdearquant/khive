//! KgPack struct, `Pack` trait impl, and pack-extensible edge endpoint rules.
//!
//! Extends the base allowlist with person-org and org-org relationship pairs (additive only).

use khive_types::{EdgeEndpointRule, EdgeRelation, EndpointKind, HandlerDef, Pack};

use crate::handler_defs::KG_HANDLERS;

/// Pack-extensible edge endpoint rules for the KG pack.
///
/// Extends the base entity→entity allowlist with person→org and org→org pairs
/// so that membership and organizational hierarchy can be expressed directly.
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
