//! Code pack vocabulary: governed value sets, the `finding` note kind spec,
//! and additive `EDGE_RULES` over the closed relation set.

use khive_runtime::{NoteKindSpec, NoteLifecycleSpec};
use khive_types::{
    EdgeEndpointRule, EdgeRelation, EndpointKind, HandlerDef, ParamDef, VerbCategory, Visibility,
};

/// `code.ingest` — the pack's first verb (ADR-085 Amendment 2 B1).
pub(crate) static CODE_HANDLERS: [HandlerDef; 1] = [HandlerDef {
    name: "code.ingest",
    description: "Ingest L1 manifest edges and L1.5 regex import-scan edges from a source \
                   folder into a dedicated map database (never the shared production graph). \
                   L2 Scanner/Extractor symbol-tier ingest is not implemented by this call.",
    visibility: Visibility::Verb,
    category: VerbCategory::Commissive,
    params: &[
        ParamDef {
            name: "path",
            param_type: "string",
            required: true,
            description: "Folder to ingest — a monorepo subtree (a single crate/package) is \
                           first-class, not a special case of whole-repo ingest.",
        },
        ParamDef {
            name: "db",
            param_type: "string",
            required: false,
            description: "Target map database path. Defaults to <path>/.khive/code-map.db. \
                           The shared production database is always rejected, with no override.",
        },
        ParamDef {
            name: "languages",
            param_type: "array of string",
            required: false,
            description: "Restrict ingest to a subset of rust | python | typescript. Defaults \
                           to all three (auto-detected from manifests found under path).",
        },
    ],
}];

pub(crate) const VALID_SEVERITIES: &[&str] = &["critical", "high", "medium", "low", "info"];
pub(crate) const VALID_CONFIDENCES: &[&str] = &["high", "medium", "low"];
pub(crate) const VALID_FINDING_STATUSES: &[&str] = &["open", "resolved", "wontfix", "invalid"];

pub(crate) fn is_valid_severity(value: &str) -> bool {
    VALID_SEVERITIES.contains(&value)
}

pub(crate) fn is_valid_confidence(value: &str) -> bool {
    VALID_CONFIDENCES.contains(&value)
}

pub(crate) fn is_valid_finding_status(value: &str) -> bool {
    VALID_FINDING_STATUSES.contains(&value)
}

/// `finding` note kind: an epistemic observation attached to a `project` (or
/// code-subtype) entity, not an entity itself. ADR-085 D4.
pub(crate) static CODE_NOTE_KIND_SPECS: [NoteKindSpec; 1] = [NoteKindSpec {
    kind: "finding",
    aliases: &["defect"],
    lifecycle: NoteLifecycleSpec {
        field: "kind_status",
        initial: "open",
        terminal: &["resolved", "wontfix", "invalid"],
        transitions: &[
            ("open", "resolved"),
            ("open", "wontfix"),
            ("open", "invalid"),
        ],
    },
}];

macro_rules! code_ep {
    ($entity_type:literal) => {
        EndpointKind::EntityOfType {
            kind: "concept",
            entity_type: $entity_type,
        }
    };
}

/// Additive edge endpoint rules for the code ontology (ADR-085 D3).
///
/// Every row uses an existing `EdgeRelation` variant; the closed relation
/// enum is never extended. Rows 1-16 are code-pack-specific; rows 17-22 are
/// base-covered but declared here for introspection per ADR-085.
pub(crate) static CODE_EDGE_RULES: [EdgeEndpointRule; 22] = [
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: code_ep!("function"),
        target: code_ep!("function"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: code_ep!("function"),
        target: code_ep!("datatype"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: code_ep!("function"),
        target: code_ep!("interface"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: code_ep!("datatype"),
        target: code_ep!("datatype"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: code_ep!("datatype"),
        target: code_ep!("interface"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: code_ep!("interface"),
        target: code_ep!("interface"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: code_ep!("interface"),
        target: code_ep!("datatype"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: code_ep!("module"),
        target: code_ep!("module"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: EndpointKind::EntityOfKind("project"),
        target: code_ep!("module"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: EndpointKind::EntityOfKind("project"),
        target: code_ep!("function"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: EndpointKind::EntityOfKind("project"),
        target: code_ep!("datatype"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: EndpointKind::EntityOfKind("project"),
        target: code_ep!("interface"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Implements,
        source: code_ep!("datatype"),
        target: code_ep!("interface"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Implements,
        source: code_ep!("function"),
        target: EndpointKind::EntityOfKind("concept"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Implements,
        source: code_ep!("datatype"),
        target: EndpointKind::EntityOfKind("concept"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Implements,
        source: code_ep!("module"),
        target: EndpointKind::EntityOfKind("concept"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: code_ep!("module"),
        target: code_ep!("module"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: code_ep!("module"),
        target: code_ep!("function"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: code_ep!("module"),
        target: code_ep!("datatype"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: code_ep!("module"),
        target: code_ep!("interface"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Extends,
        source: code_ep!("interface"),
        target: code_ep!("interface"),
    },
    EdgeEndpointRule {
        relation: EdgeRelation::Extends,
        source: code_ep!("datatype"),
        target: code_ep!("datatype"),
    },
];

#[cfg(test)]
mod tests {
    use khive_types::{EdgeRelation, EndpointKind};

    use super::CODE_EDGE_RULES;

    #[test]
    fn code_edge_rules_has_22_rows() {
        assert_eq!(CODE_EDGE_RULES.len(), 22);
    }

    #[test]
    fn code_edge_rules_contains_function_depends_on_datatype() {
        let found = CODE_EDGE_RULES.iter().any(|r| {
            r.relation == EdgeRelation::DependsOn
                && r.source
                    == EndpointKind::EntityOfType {
                        kind: "concept",
                        entity_type: "function",
                    }
                && r.target
                    == EndpointKind::EntityOfType {
                        kind: "concept",
                        entity_type: "datatype",
                    }
        });
        assert!(found, "must contain function depends_on datatype");
    }

    #[test]
    fn code_edge_rules_contains_project_contains_module() {
        let found = CODE_EDGE_RULES.iter().any(|r| {
            r.relation == EdgeRelation::Contains
                && r.source == EndpointKind::EntityOfKind("project")
                && r.target
                    == EndpointKind::EntityOfType {
                        kind: "concept",
                        entity_type: "module",
                    }
        });
        assert!(found, "must contain project contains module");
    }
}
