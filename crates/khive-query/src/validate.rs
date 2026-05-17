//! AST validation per ADR-008 §Validation Rules.
//!
//! `validate` normalises an AST in place and rejects queries that violate the
//! closed edge ontology or attempt to subvert namespace scoping:
//!
//! 1. **Edge relations** must parse to one of the 13 canonical [`EdgeRelation`]
//!    variants (ADR-002). Aliases and case differences are normalised to the
//!    canonical snake_case form stored in the database. Applies to edge
//!    patterns *and* `WHERE e.relation = '…'` constraints.
//! 2. **Node kinds** pass through unchanged — the query layer is pack-agnostic
//!    (ADR-025). Kind validation is the responsibility of the service boundary,
//!    not the query compiler.
//! 3. **Namespace scoping is a trusted parameter only.** Queries must not name
//!    `namespace` in node property maps or `WHERE` conditions — the only valid
//!    source of namespace filtering is `CompileOptions::scopes`. This matches
//!    ADR-008 §Validation: "never trust query strings to set namespaces."
//! 4. **Traversal depth** is capped at [`MAX_DEPTH`] (10 hops). Requests above
//!    the cap are clamped, not rejected — this matches the cap the compiler
//!    applies when generating recursive CTEs.

use std::collections::HashSet;
use std::str::FromStr;

use khive_types::EdgeRelation;

use crate::ast::{Condition, ConditionValue, GqlQuery, PatternElement};
use crate::error::QueryError;

/// Maximum traversal depth allowed by the query layer (ADR-008 §Validation).
pub const MAX_DEPTH: usize = 10;

/// Validate and normalise an AST in place.
///
/// Canonicalizes edge relation strings to their snake_case form (closed set).
/// Node kind strings pass through unchanged (pack-agnostic).
pub fn validate(query: &mut GqlQuery) -> Result<(), QueryError> {
    // Pattern variables are bindings — the same variable name appearing twice
    // would mean "same node/edge" and require alias-equality predicates in
    // SQL. Until that is implemented, reject repeated bindings explicitly so
    // cycles and self-reachability don't silently compile to wrong results.
    let mut seen_node_vars: HashSet<&str> = HashSet::new();
    let mut seen_edge_vars: HashSet<&str> = HashSet::new();
    for element in &query.pattern.elements {
        match element {
            PatternElement::Node(node) => {
                if let Some(var) = node.variable.as_deref() {
                    if !seen_node_vars.insert(var) {
                        return Err(QueryError::Unsupported(format!(
                            "repeated node variable '{var}' (cycle / self-reachability \
                             requires alias-equality predicates not yet implemented)"
                        )));
                    }
                }
            }
            PatternElement::Edge(edge) => {
                if let Some(var) = edge.variable.as_deref() {
                    if !seen_edge_vars.insert(var) {
                        return Err(QueryError::Unsupported(format!(
                            "repeated edge variable '{var}' not supported"
                        )));
                    }
                }
            }
        }
    }

    for element in &mut query.pattern.elements {
        match element {
            PatternElement::Node(node) => {
                if node.properties.contains_key("namespace") {
                    return Err(QueryError::Validation(
                        "namespace is set by CompileOptions, not query text".into(),
                    ));
                }
            }
            PatternElement::Edge(edge) => {
                for relation in edge.relations.iter_mut() {
                    let parsed = EdgeRelation::from_str(relation)
                        .map_err(|err| QueryError::Validation(err.to_string()))?;
                    *relation = parsed.as_str().to_string();
                }
                if edge.min_hops == 0 {
                    return Err(QueryError::Unsupported(
                        "zero-hop ranges (min_hops = 0) not yet supported; \
                         use a minimum of 1 hop"
                            .into(),
                    ));
                }
                // Reject inverted ranges before any clamping — silently
                // rewriting *3..1 to *1..1 changes query semantics.
                if edge.min_hops > edge.max_hops {
                    return Err(QueryError::Validation(format!(
                        "invalid hop range: min {} > max {}",
                        edge.min_hops, edge.max_hops
                    )));
                }
                // If the minimum already exceeds our depth cap, the query
                // can never produce results — reject rather than silently
                // returning an empty set from a clamped range.
                if edge.min_hops > MAX_DEPTH {
                    return Err(QueryError::Unsupported(format!(
                        "minimum hop count {} exceeds depth cap {}",
                        edge.min_hops, MAX_DEPTH
                    )));
                }
                // Clamp max_hops to the depth cap — the lower bound is
                // still satisfiable, so this only narrows the search.
                if edge.max_hops > MAX_DEPTH {
                    edge.max_hops = MAX_DEPTH;
                }
            }
        }
    }

    // Build variable → kind map so condition validation is context-aware.
    // `kind` and `relation` only get taxonomy enforcement on the correct
    // variable type (node vs edge). On the other type, they're treated as
    // ordinary JSON property keys.
    let mut var_kinds: std::collections::HashMap<&str, VarKind> = std::collections::HashMap::new();
    for element in &query.pattern.elements {
        match element {
            PatternElement::Node(n) => {
                if let Some(v) = n.variable.as_deref() {
                    var_kinds.insert(v, VarKind::Node);
                }
            }
            PatternElement::Edge(e) => {
                if let Some(v) = e.variable.as_deref() {
                    var_kinds.insert(v, VarKind::Edge);
                }
            }
        }
    }

    for cond in query.where_clause.iter_mut() {
        let is_edge = var_kinds
            .get(cond.variable.as_str())
            .copied()
            .unwrap_or(VarKind::Node)
            == VarKind::Edge;
        validate_condition(cond, is_edge)?;
    }

    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VarKind {
    Node,
    Edge,
}

fn validate_condition(cond: &mut Condition, is_edge: bool) -> Result<(), QueryError> {
    match cond.property.as_str() {
        "namespace" => Err(QueryError::Validation(
            "namespace is set by CompileOptions, not query text".into(),
        )),
        "kind" if !is_edge => Ok(()),
        "relation" if is_edge => {
            if let ConditionValue::String(ref mut s) = cond.value {
                let parsed = EdgeRelation::from_str(s)
                    .map_err(|err| QueryError::Validation(err.to_string()))?;
                *s = parsed.as_str().to_string();
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsers::gql;

    #[test]
    fn node_kind_passes_through_unchanged() {
        // Entity kinds are pack-agnostic strings — no normalization at the query layer.
        let mut q = gql::parse("MATCH (a:paper)-[:introduced_by]->(b:concept) RETURN a").unwrap();
        validate(&mut q).unwrap();
        let kinds: Vec<_> = q
            .pattern
            .nodes()
            .map(|n| n.kind.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(kinds, vec!["paper", "concept"]);
    }

    #[test]
    fn normalises_relation_case_and_hyphens() {
        let mut q = gql::parse("MATCH (a)-[:Introduced_By]->(b) RETURN a").unwrap();
        validate(&mut q).unwrap();
        let rels: Vec<_> = q
            .pattern
            .edges()
            .flat_map(|e| e.relations.iter().cloned())
            .collect();
        assert_eq!(rels, vec!["introduced_by".to_string()]);
    }

    #[test]
    fn rejects_unknown_relation() {
        let mut q = gql::parse("MATCH (a)-[:not_a_relation]->(b) RETURN a").unwrap();
        let err = validate(&mut q).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not_a_relation"), "msg: {msg}");
    }

    #[test]
    fn unknown_kind_passes_through() {
        // Entity kinds are pack-agnostic strings — any string is accepted at the query layer.
        let mut q = gql::parse("MATCH (a:gizmo)-[:extends]->(b) RETURN a").unwrap();
        validate(&mut q).unwrap();
    }

    #[test]
    fn clamps_depth_above_max() {
        let mut q = gql::parse("MATCH (a)-[:extends*1..50]->(b) RETURN b").unwrap();
        validate(&mut q).unwrap();
        let edge = q.pattern.edges().next().unwrap();
        assert_eq!(edge.max_hops, MAX_DEPTH);
        assert!(edge.min_hops <= edge.max_hops);
    }

    #[test]
    fn multi_relation_all_normalised() {
        let mut q = gql::parse("MATCH (a)-[:Extends|VARIANT_OF]->(b) RETURN a").unwrap();
        validate(&mut q).unwrap();
        let edge = q.pattern.edges().next().unwrap();
        assert_eq!(
            edge.relations,
            vec!["extends".to_string(), "variant_of".to_string()]
        );
    }

    #[test]
    fn rejects_namespace_in_where() {
        let mut q =
            gql::parse("MATCH (a:concept)-[:extends]->(b) WHERE a.namespace = 'other' RETURN a")
                .unwrap();
        let err = validate(&mut q).unwrap_err();
        assert!(err.to_string().contains("namespace"), "msg: {err}");
    }

    #[test]
    fn rejects_namespace_in_node_properties() {
        let mut q =
            gql::parse("MATCH (a:concept {namespace: 'other'})-[:extends]->(b) RETURN a").unwrap();
        let err = validate(&mut q).unwrap_err();
        assert!(err.to_string().contains("namespace"), "msg: {err}");
    }

    #[test]
    fn rejects_unknown_relation_in_where() {
        let mut q =
            gql::parse("MATCH (a)-[e:extends]->(b) WHERE e.relation = 'related_to' RETURN a")
                .unwrap();
        let err = validate(&mut q).unwrap_err();
        assert!(err.to_string().contains("related_to"), "msg: {err}");
    }

    #[test]
    fn unknown_kind_in_where_passes_through() {
        // Entity kinds are pack-agnostic strings — any kind string is accepted.
        let mut q =
            gql::parse("MATCH (a)-[:extends]->(b) WHERE a.kind = 'gizmo' RETURN a").unwrap();
        validate(&mut q).unwrap();
        let val = match &q.where_clause[0].value {
            ConditionValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert_eq!(val, "gizmo");
    }

    #[test]
    fn kind_in_where_passes_through_unchanged() {
        // Pack-agnostic: 'paper' is not normalized to 'document'; strings pass through as-is.
        let mut q =
            gql::parse("MATCH (a)-[:extends]->(b) WHERE a.kind = 'paper' RETURN a").unwrap();
        validate(&mut q).unwrap();
        let val = match &q.where_clause[0].value {
            ConditionValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert_eq!(val, "paper");
    }

    #[test]
    fn normalises_relation_alias_in_where() {
        let mut q =
            gql::parse("MATCH (a)-[e:extends]->(b) WHERE e.relation = 'Introduced_By' RETURN a")
                .unwrap();
        validate(&mut q).unwrap();
        let val = match &q.where_clause[0].value {
            ConditionValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert_eq!(val, "introduced_by");
    }

    #[test]
    fn rejects_zero_hop_range_gql_wide() {
        let mut q = gql::parse("MATCH (a)-[:extends*0..3]->(b) RETURN b").unwrap();
        let err = validate(&mut q).unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "expected Unsupported, got {err:?}"
        );
    }

    #[test]
    fn rejects_zero_hop_range_gql_narrow() {
        // *0..1 has max_hops=1 so has_variable_length() is false, but the
        // fixed-length compiler also can't produce zero-hop rows — reject at
        // validation regardless of compile path.
        let mut q = gql::parse("MATCH (a)-[:extends*0..1]->(b) RETURN b").unwrap();
        let err = validate(&mut q).unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "expected Unsupported, got {err:?}"
        );
    }

    #[test]
    fn rejects_zero_hop_sparql_explicit_range() {
        use crate::parsers::sparql;
        let mut q = sparql::parse("SELECT ?a ?b WHERE { ?a :extends{0,3} ?b . }").unwrap();
        let err = validate(&mut q).unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "expected Unsupported, got {err:?}"
        );
    }

    #[test]
    fn rejects_repeated_node_var_cycle_gql() {
        let mut q = gql::parse("MATCH (a)-[:extends]->(b)-[:variant_of]->(a) RETURN a").unwrap();
        let err = validate(&mut q).unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "expected Unsupported, got {err:?}"
        );
    }

    #[test]
    fn rejects_repeated_node_var_self_reach_variable_length() {
        let mut q = gql::parse("MATCH (a)-[:extends*1..3]->(a) RETURN a").unwrap();
        let err = validate(&mut q).unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "expected Unsupported, got {err:?}"
        );
    }

    #[test]
    fn rejects_repeated_node_var_cycle_sparql() {
        use crate::parsers::sparql;
        let mut q =
            sparql::parse("SELECT ?a WHERE { ?a :extends ?b . ?b :variant_of ?a . }").unwrap();
        let err = validate(&mut q).unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "expected Unsupported, got {err:?}"
        );
    }

    #[test]
    fn rejects_repeated_edge_var() {
        let mut q = gql::parse("MATCH (a)-[e:extends]->(b)-[e:variant_of]->(c) RETURN c").unwrap();
        let err = validate(&mut q).unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "expected Unsupported, got {err:?}"
        );
    }

    #[test]
    fn rejects_inverted_range() {
        // *3..1 is an inverted range — must error, not silently rewrite to *1..1.
        let mut q = gql::parse("MATCH (a)-[:extends*3..1]->(b) RETURN b").unwrap();
        let err = validate(&mut q).unwrap_err();
        assert!(
            matches!(err, QueryError::Validation(_)),
            "expected Validation error, got {err:?}"
        );
    }

    #[test]
    fn rejects_min_hops_above_depth_cap() {
        // min=50, max=100 — the lower bound exceeds MAX_DEPTH so the query
        // can never produce results within our cap.
        let mut q = gql::parse("MATCH (a)-[:extends*50..100]->(b) RETURN b").unwrap();
        let err = validate(&mut q).unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "expected Unsupported, got {err:?}"
        );
    }

    #[test]
    fn clamps_max_but_keeps_satisfiable_min() {
        // *2..50 — min 2 is satisfiable, max gets clamped to MAX_DEPTH.
        let mut q = gql::parse("MATCH (a)-[:extends*2..50]->(b) RETURN b").unwrap();
        validate(&mut q).unwrap();
        let edge = q.pattern.edges().next().unwrap();
        assert_eq!(edge.min_hops, 2);
        assert_eq!(edge.max_hops, MAX_DEPTH);
    }

    #[test]
    fn node_property_named_relation_allowed() {
        // `relation` on a node variable is a free-form JSON property, not the
        // edge relation column — taxonomy enforcement should not apply.
        let mut q =
            gql::parse("MATCH (a)-[:extends]->(b) WHERE a.relation = 'external' RETURN a").unwrap();
        validate(&mut q).unwrap();
        let val = match &q.where_clause[0].value {
            ConditionValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert_eq!(val, "external");
    }

    #[test]
    fn edge_relation_still_validated() {
        // `relation` on an edge variable must still go through EdgeRelation
        // taxonomy validation.
        let mut q = gql::parse("MATCH (a)-[e:extends]->(b) WHERE e.relation = 'not_real' RETURN a")
            .unwrap();
        let err = validate(&mut q).unwrap_err();
        assert!(err.to_string().contains("not_real"), "msg: {err}");
    }
}
