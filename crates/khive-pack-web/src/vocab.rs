use khive_types::{
    EdgeEndpointRule, EdgeRelation, EndpointKind, HandlerDef, IdResolutionMode, ParamDef,
    VerbCategory, Visibility,
};

pub(crate) static WEB_HANDLERS: [HandlerDef; 1] = [HandlerDef {
    name: "web.ingest",
    description: "Ingest a local web origin's declared manifest and optional machine views \
                  into a dedicated map database. Reads local files only and never writes \
                  the shared production graph.",
    visibility: Visibility::Verb,
    category: VerbCategory::Commissive,
    params: &[
        ParamDef {
            name: "source",
            param_type: "string",
            required: true,
            description: "Local directory containing the served site tree and the required \
                          .well-known/arw.json manifest. Network URLs are not accepted.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "db",
            param_type: "string",
            required: false,
            description: "Target map database path. Defaults to <source>/.khive/web-map.db. \
                          The shared production database is always rejected, with no override.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
        ParamDef {
            name: "include_views",
            param_type: "boolean",
            required: false,
            description: "Defaults to true. False skips machine-view files and creates no \
                          machine_view entities or derived_from edges; page counts are unchanged.",
            resolution_mode: IdResolutionMode::NotApplicable,
        },
    ],
}];

// Derivation and implementation restate base rules for introspection (ADR-175 D3).
pub(crate) static WEB_EDGE_RULES: [EdgeEndpointRule; 6] = [
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: EndpointKind::EntityOfType {
            kind: "service",
            entity_type: "site",
        },
        target: EndpointKind::EntityOfType {
            kind: "document",
            entity_type: "page",
        },
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: EndpointKind::EntityOfType {
            kind: "service",
            entity_type: "site",
        },
        target: EndpointKind::EntityOfType {
            kind: "service",
            entity_type: "agent_tool",
        },
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: EndpointKind::EntityOfType {
            kind: "service",
            entity_type: "site",
        },
        target: EndpointKind::EntityOfType {
            kind: "document",
            entity_type: "agent_skill",
        },
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DerivedFrom,
        source: EndpointKind::EntityOfType {
            kind: "document",
            entity_type: "machine_view",
        },
        target: EndpointKind::EntityOfType {
            kind: "document",
            entity_type: "page",
        },
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::EntityOfType {
            kind: "document",
            entity_type: "agent_skill",
        },
        target: EndpointKind::EntityOfType {
            kind: "service",
            entity_type: "agent_tool",
        },
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Implements,
        source: EndpointKind::EntityOfType {
            kind: "service",
            entity_type: "site",
        },
        target: EndpointKind::EntityOfType {
            kind: "concept",
            entity_type: "interface",
        },
    },
];
