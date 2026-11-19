use khive_types::{EdgeRelation, EndpointKind};

use super::{CODE_EDGE_RULES, CODE_HANDLERS};

#[test]
fn code_ingest_declares_optional_tiers() {
    let tiers = CODE_HANDLERS[0]
        .params
        .iter()
        .find(|param| param.name == "tiers")
        .expect("code.ingest must declare the tiers parameter");

    assert_eq!(tiers.param_type, "array of string");
    assert!(!tiers.required);
}

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
