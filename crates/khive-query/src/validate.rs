//! AST validation per ADR-008 §Validation Rules.
//!
//! `validate` normalises an AST in place and rejects queries that violate the
//! closed taxonomies or attempt to subvert namespace scoping:
//!
//! 1. **Edge relations** must parse to one of the 13 canonical [`EdgeRelation`]
//!    variants (ADR-002). Aliases and case differences are normalised to the
//!    canonical snake_case form stored in the database. Applies to edge
//!    patterns *and* `WHERE e.relation = '…'` constraints.
//! 2. **Node kinds** must parse to one of the 6 [`EntityKind`] variants
//!    (ADR-001). Common aliases (`paper` → `document`, `benchmark` → `dataset`)
//!    are normalised. Applies to node labels *and* `WHERE a.kind = '…'`
//!    constraints.
//! 3. **Namespace scoping is a trusted parameter only.** Queries must not name
//!    `namespace` in node property maps or `WHERE` conditions — the only valid
//!    source of namespace filtering is `CompileOptions::scopes`. This matches
//!    ADR-008 §Validation: "never trust query strings to set namespaces."
//! 4. **Traversal depth** is capped at [`MAX_DEPTH`] (10 hops). Requests above
//!    the cap are clamped, not rejected — this matches the cap the compiler
//!    applies when generating recursive CTEs.

use std::str::FromStr;

use khive_types::{EdgeRelation, EntityKind};

use crate::ast::{Condition, ConditionValue, GqlQuery, PatternElement};
use crate::error::QueryError;

/// Maximum traversal depth allowed by the query layer (ADR-008 §Validation).
pub const MAX_DEPTH: usize = 10;

/// Validate and normalise an AST in place.
///
/// On success, every kind / relation string in the AST is replaced with its
/// canonical lowercase form so the compiler can emit literal SQL parameters
/// that match the values written by `khive-db`.
pub fn validate(query: &mut GqlQuery) -> Result<(), QueryError> {
    for element in &mut query.pattern.elements {
        match element {
            PatternElement::Node(node) => {
                if let Some(kind) = node.kind.as_mut() {
                    let parsed = EntityKind::from_str(kind).map_err(QueryError::Validation)?;
                    *kind = parsed.name().to_string();
                }
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
                if edge.max_hops > MAX_DEPTH {
                    edge.max_hops = MAX_DEPTH;
                }
                if edge.min_hops > edge.max_hops {
                    edge.min_hops = edge.max_hops;
                }
                // Zero-hop (start == end) results require a depth-0 seed in
                // the recursive CTE that we haven't implemented yet. Reject
                // explicitly rather than silently compiling as one-or-more.
                if edge.min_hops == 0 {
                    return Err(QueryError::Unsupported(
                        "zero-hop ranges (min_hops = 0) not yet supported; \
                         use a minimum of 1 hop"
                            .into(),
                    ));
                }
            }
        }
    }

    for cond in query.where_clause.iter_mut() {
        validate_condition(cond)?;
    }

    Ok(())
}

fn validate_condition(cond: &mut Condition) -> Result<(), QueryError> {
    match cond.property.as_str() {
        "namespace" => Err(QueryError::Validation(
            "namespace is set by CompileOptions, not query text".into(),
        )),
        "kind" => {
            if let ConditionValue::String(ref mut s) = cond.value {
                let parsed = EntityKind::from_str(s).map_err(QueryError::Validation)?;
                *s = parsed.name().to_string();
            }
            Ok(())
        }
        "relation" => {
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
    fn normalises_entity_kind_aliases() {
        let mut q = gql::parse("MATCH (a:paper)-[:introduced_by]->(b:concept) RETURN a").unwrap();
        validate(&mut q).unwrap();
        let kinds: Vec<_> = q
            .pattern
            .nodes()
            .map(|n| n.kind.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(kinds, vec!["document", "concept"]);
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
    fn rejects_unknown_kind() {
        let mut q = gql::parse("MATCH (a:gizmo)-[:extends]->(b) RETURN a").unwrap();
        let err = validate(&mut q).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("gizmo"), "msg: {msg}");
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
    fn rejects_unknown_kind_in_where() {
        let mut q =
            gql::parse("MATCH (a)-[:extends]->(b) WHERE a.kind = 'gizmo' RETURN a").unwrap();
        let err = validate(&mut q).unwrap_err();
        assert!(err.to_string().contains("gizmo"), "msg: {err}");
    }

    #[test]
    fn normalises_kind_alias_in_where() {
        let mut q =
            gql::parse("MATCH (a)-[:extends]->(b) WHERE a.kind = 'paper' RETURN a").unwrap();
        validate(&mut q).unwrap();
        let val = match &q.where_clause[0].value {
            ConditionValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert_eq!(val, "document");
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
}
