//! Workspace entity kind and additive membership endpoint rules.

use khive_types::{EdgeEndpointRule, EdgeRelation, EndpointKind};

/// Entity kinds this pack contributes to the runtime vocabulary.
pub(crate) const ENTITY_KINDS: &[&str] = &["workspace"];

/// `workspace -[contains]->` git, task, and session note endpoints.
///
/// Document membership remains deferred. See
/// `crates/khive-pack-workspace/docs/api/workspace-registration.md`.
pub(crate) static WORKSPACE_EDGE_RULES: [EdgeEndpointRule; 5] = [
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: EndpointKind::EntityOfKind("workspace"),
        target: EndpointKind::NoteOfKind("issue"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: EndpointKind::EntityOfKind("workspace"),
        target: EndpointKind::NoteOfKind("pull_request"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: EndpointKind::EntityOfKind("workspace"),
        target: EndpointKind::NoteOfKind("commit"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: EndpointKind::EntityOfKind("workspace"),
        target: EndpointKind::NoteOfKind("task"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: EndpointKind::EntityOfKind("workspace"),
        target: EndpointKind::NoteOfKind("session"),
    },
];
