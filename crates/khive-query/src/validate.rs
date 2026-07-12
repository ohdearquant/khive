//! AST validation: relation normalization, namespace guard, depth cap.

use std::collections::HashSet;
use std::str::FromStr;

use khive_types::EdgeRelation;

use crate::ast::{CompareOp, Condition, ConditionValue, GqlQuery, PatternElement};
use crate::error::QueryError;

/// Valid synthetic relations; unknown `observed_as_*` strings are rejected.
const SYNTHETIC_RELATIONS: &[&str] = &[
    "observed_as_candidate",
    "observed_as_selected",
    "observed_as_target",
    "observed_as_signal",
];

/// Maximum traversal depth allowed by the query layer.
pub const MAX_DEPTH: usize = 10;

/// Validate and normalise an AST in place.
pub fn validate(query: &mut GqlQuery) -> Result<(), QueryError> {
    validate_with_warnings(query).map(|_| ())
}

/// Validate that a pattern alternates Node/Edge/Node correctly.
pub fn validate_pattern_shape(elements: &[PatternElement]) -> Result<(), QueryError> {
    if elements.is_empty() {
        // Empty pattern: caught separately by the compiler as "empty pattern".
        return Ok(());
    }
    if elements.len().is_multiple_of(2) {
        return Err(QueryError::Validation(
            "pattern must alternate Node, Edge, Node, … (even element count is invalid)".into(),
        ));
    }
    for (i, element) in elements.iter().enumerate() {
        match (i % 2, element) {
            (0, PatternElement::Node(_)) => {}
            (1, PatternElement::Edge(_)) => {}
            _ => {
                return Err(QueryError::Validation(
                    "pattern must alternate Node, Edge, Node, … (wrong element type at position)"
                        .into(),
                ))
            }
        }
    }
    Ok(())
}

/// Validate and normalise an AST in place, returning any warnings generated.
pub fn validate_with_warnings(query: &mut GqlQuery) -> Result<Vec<String>, QueryError> {
    let warnings: Vec<String> = Vec::new();

    // Structural shape check: must alternate Node/Edge/Node.
    validate_pattern_shape(&query.pattern.elements)?;

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
                    // Synthetic observed_as_* relations do not exist in the
                    // closed EdgeRelation enum — skip taxonomy validation and
                    // leave the string unchanged.  The SQL compiler handles them
                    // via the event_observations join path.
                    // Only the four known synthetic relations are valid; an unknown
                    // observed_as_* string must be rejected (closes the bypass that
                    // allowed arbitrary observed_as_bogus strings to compile as
                    // canonical graph_edges queries).
                    if relation.starts_with("observed_as_") {
                        if !SYNTHETIC_RELATIONS.contains(&relation.as_str()) {
                            return Err(QueryError::Validation(format!(
                                "unknown synthetic relation '{relation}'; valid synthetic relations: {}",
                                SYNTHETIC_RELATIONS.join(", ")
                            )));
                        }
                        continue;
                    }
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
                // Reject max_hops above the depth cap.
                if edge.max_hops > MAX_DEPTH {
                    return Err(QueryError::InvalidInput(format!(
                        "max_hops {} exceeds the depth cap of {}; reduce the range or use a smaller bound",
                        edge.max_hops, MAX_DEPTH
                    )));
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

    // Walk all leaf conditions in the WHERE expression tree.
    let mut validate_err: Option<QueryError> = None;
    query.where_clause.for_each_condition_mut(&mut |cond| {
        if validate_err.is_some() {
            return;
        }
        let is_edge = var_kinds
            .get(cond.variable.as_str())
            .copied()
            .unwrap_or(VarKind::Node)
            == VarKind::Edge;
        if let Err(e) = validate_condition(cond, is_edge) {
            validate_err = Some(e);
        }
    });
    if let Some(e) = validate_err {
        return Err(e);
    }

    Ok(warnings)
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
            let normalize = |s: &mut String| -> Result<(), QueryError> {
                let parsed = EdgeRelation::from_str(s)
                    .map_err(|err| QueryError::Validation(err.to_string()))?;
                *s = parsed.as_str().to_string();
                Ok(())
            };
            if matches!(
                cond.op,
                CompareOp::Contains | CompareOp::StartsWith | CompareOp::IsNotNull
            ) {
                return Ok(());
            }
            match &mut cond.value {
                ConditionValue::String(s) => normalize(s)?,
                ConditionValue::List(values) => {
                    for value in values {
                        match value {
                            ConditionValue::String(s) => normalize(s)?,
                            _ => {
                                return Err(QueryError::Validation(
                                    "relation IN list values must be strings".into(),
                                ));
                            }
                        }
                    }
                }
                _ => {}
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
#[path = "validate_tests.rs"]
mod tests;
