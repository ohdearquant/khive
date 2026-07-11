//! Workspace pack vocabulary: the `workspace` entity kind and its five
//! additive `contains` endpoint rules.

use khive_types::{EdgeEndpointRule, EdgeRelation, EndpointKind};

/// Entity kinds this pack contributes to the runtime vocabulary.
pub(crate) const ENTITY_KINDS: &[&str] = &["workspace"];

/// v0 membership edges: `workspace -[contains]-> member`, one rule per
/// already-shipped member note kind (git's `issue`/`pull_request`/`commit`,
/// GTD's `task`, session's `session`). All additive  -  the base contract
/// treats `contains` as entity-to-entity only; these rules broaden it to
/// entity-to-note for the five kinds a workspace can hold in v0. Document
/// membership (pack-doc #872) is deliberately absent until that pack settles
/// its substrate contract.
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
