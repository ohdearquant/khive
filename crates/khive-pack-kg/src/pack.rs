//! KgPack struct, `Pack` trait impl, and pack-extensible edge endpoint rules.

use khive_types::{EdgeEndpointRule, EdgeRelation, EndpointKind, HandlerDef, Pack};

use crate::handler_defs::KG_HANDLERS;

/// Pack-extensible edge endpoint rules (person↔org, person↔project, org↔org).
/// See `docs/api/edge-rules-pack-kg.md`.
///
/// ADR-087 Amendment 1 §A9: decision review rounds chain via
/// `decision precedes decision` note-to-note edges. Round N-1 precedes round
/// N; this is NOT `supersedes` — earlier rounds stay authoritative for their
/// own commit. The base `precedes` contract (ADR-002) is entity-to-entity
/// only; this rule additively extends it to `decision` note pairs, mirroring
/// the GTD pack's `task`->`task` `depends_on` rule.
pub(crate) static KG_EDGE_RULES: [EdgeEndpointRule; 10] = [
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
        relation: EdgeRelation::PartOf,
        source: EndpointKind::EntityOfKind("person"),
        target: EndpointKind::EntityOfKind("project"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::InstanceOf,
        source: EndpointKind::EntityOfKind("person"),
        target: EndpointKind::EntityOfKind("project"),
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
    EdgeEndpointRule {
        relation: EdgeRelation::Precedes,
        source: EndpointKind::NoteOfKind("decision"),
        target: EndpointKind::NoteOfKind("decision"),
    },
];

/// KG pack vocabulary declaration.
pub struct KgPack {
    pub(crate) runtime: khive_runtime::KhiveRuntime,
}

impl Pack for KgPack {
    const NAME: &'static str = "kg";
    const NOTE_KINDS: &'static [&'static str] = crate::vocab::NoteKind::NAMES;
    const ENTITY_KINDS: &'static [&'static str] = &[
        "concept", "document", "dataset", "project", "person", "org", "artifact", "service",
        "resource",
    ];
    // ADR-086 Amendment 1, Decision 1.
    const ENTITY_TYPES: &'static [khive_types::EntityTypeDef] = &[
        khive_types::EntityTypeDef {
            kind: khive_types::EntityKind::Document,
            type_name: "note",
            aliases: &[],
        },
        khive_types::EntityTypeDef {
            kind: khive_types::EntityKind::Document,
            type_name: "summary",
            aliases: &[],
        },
        khive_types::EntityTypeDef {
            kind: khive_types::EntityKind::Document,
            type_name: "handoff",
            aliases: &[],
        },
        khive_types::EntityTypeDef {
            kind: khive_types::EntityKind::Document,
            type_name: "transcript",
            aliases: &[],
        },
        khive_types::EntityTypeDef {
            kind: khive_types::EntityKind::Document,
            type_name: "other",
            aliases: &[],
        },
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
#[path = "pack_tests.rs"]
mod tests;
