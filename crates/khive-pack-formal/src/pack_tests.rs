use khive_types::{EdgeRelation, EndpointKind};

use super::FORMAL_EDGE_RULES;

#[test]
fn formal_edge_rules_contains_variant_of_theorem_to_theorem() {
    let found = FORMAL_EDGE_RULES.iter().any(|r| {
        r.relation == EdgeRelation::VariantOf
            && r.source
                == EndpointKind::EntityOfType {
                    kind: "concept",
                    entity_type: "theorem",
                }
            && r.target
                == EndpointKind::EntityOfType {
                    kind: "concept",
                    entity_type: "theorem",
                }
    });
    assert!(
        found,
        "FORMAL_EDGE_RULES must contain variant_of theorem->theorem"
    );
}

#[test]
fn formal_edge_rules_contains_depends_on_goal_to_theorem() {
    let found = FORMAL_EDGE_RULES.iter().any(|r| {
        r.relation == EdgeRelation::DependsOn
            && r.source
                == EndpointKind::EntityOfType {
                    kind: "concept",
                    entity_type: "goal",
                }
            && r.target
                == EndpointKind::EntityOfType {
                    kind: "concept",
                    entity_type: "theorem",
                }
    });
    assert!(
        found,
        "FORMAL_EDGE_RULES must contain depends_on goal->theorem"
    );
}
