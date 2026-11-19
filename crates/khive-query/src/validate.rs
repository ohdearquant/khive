//! AST validation and relation normalization.

use std::collections::HashSet;
use std::str::FromStr;

use khive_types::EdgeRelation;

use crate::ast::{
    CompareOp, Condition, ConditionValue, GqlQuery, MatchPattern, PatternElement, PropertyRef,
    WhereExpr,
};
use crate::error::QueryError;

/// Closed synthetic relation set handled outside the canonical edge enum.
pub(crate) const SYNTHETIC_RELATIONS: &[&str] = &[
    "observed_as_candidate",
    "observed_as_selected",
    "observed_as_target",
    "observed_as_signal",
];

/// Maximum accepted traversal depth, in hops.
pub const MAX_DEPTH: usize = 10;

/// Validates and normalizes `query` in place.
///
/// # Errors
///
/// Returns [`QueryError::Validation`] for structural or taxonomy violations and
/// [`QueryError::InvalidInput`] for hop bounds above [`MAX_DEPTH`].
/// See `crates/khive-query/docs/api/validation.md` for the full rule set.
pub fn validate(query: &mut GqlQuery) -> Result<(), QueryError> {
    validate_with_warnings(query).map(|_| ())
}

/// Validates that a non-empty pattern alternates node/edge/node.
///
/// # Errors
///
/// Returns [`QueryError::Validation`] for an even-length or misordered pattern.
pub fn validate_pattern_shape(elements: &[PatternElement]) -> Result<(), QueryError> {
    if elements.is_empty() {
        // Compilation owns the more specific empty-pattern diagnostic.
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

/// Validates and normalizes `query`, returning non-fatal diagnostics.
///
/// # Errors
///
/// Returns the same errors as [`validate`].
/// See `crates/khive-query/docs/api/validation.md` for mutation and warning behavior.
pub fn validate_with_warnings(query: &mut GqlQuery) -> Result<Vec<String>, QueryError> {
    let warnings: Vec<String> = Vec::new();

    validate_bindings_and_connectivity(query)?;
    let multiple_patterns = !query.additional_patterns.is_empty();

    for element in query
        .patterns_mut()
        .flat_map(|pattern| &mut pattern.elements)
    {
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
                    // Synthetic projections are closed but intentionally outside EdgeRelation.
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
                // Never rewrite inverted ranges; doing so changes query semantics.
                if edge.min_hops > edge.max_hops {
                    return Err(QueryError::Validation(format!(
                        "invalid hop range: min {} > max {}",
                        edge.min_hops, edge.max_hops
                    )));
                }
                if edge.min_hops > MAX_DEPTH {
                    return Err(QueryError::Unsupported(format!(
                        "minimum hop count {} exceeds depth cap {}",
                        edge.min_hops, MAX_DEPTH
                    )));
                }
                if edge.max_hops > MAX_DEPTH {
                    return Err(QueryError::InvalidInput(format!(
                        "max_hops {} exceeds the depth cap of {}; reduce the range or use a smaller bound",
                        edge.max_hops, MAX_DEPTH
                    )));
                }
                if multiple_patterns
                    && (edge.min_hops != 1
                        || edge.max_hops != 1
                        || edge
                            .relations
                            .iter()
                            .any(|r| SYNTHETIC_RELATIONS.contains(&r.as_str())))
                {
                    return Err(QueryError::Unsupported(
                        "comma-separated MATCH requires canonical fixed one-hop edges".into(),
                    ));
                }
            }
        }
    }

    // Check the sum only after every edge's existing range/taxonomy checks, so
    // their established diagnostics retain precedence. Normalized 1..1 is fixed.
    let variable_maximum = query
        .pattern
        .edges()
        .filter(|edge| edge.min_hops != 1 || edge.max_hops != 1)
        .try_fold(0usize, |total, edge| total.checked_add(edge.max_hops))
        .ok_or_else(|| {
            QueryError::InvalidInput(format!(
                "sum of variable-segment maximum hops exceeds the depth cap of {MAX_DEPTH}"
            ))
        })?;
    if variable_maximum > MAX_DEPTH {
        return Err(QueryError::InvalidInput(format!(
            "sum of variable-segment maximum hops {variable_maximum} exceeds the depth cap of {MAX_DEPTH}; fixed steps do not count toward this sum"
        )));
    }
    if query.pattern.has_variable_length()
        && query.pattern.edges().count() > 1
        && query.pattern.edges().any(|edge| {
            edge.relations
                .iter()
                .any(|relation| SYNTHETIC_RELATIONS.contains(&relation.as_str()))
        })
    {
        return Err(QueryError::Unsupported(
            "mixed variable-length paths require canonical edges; synthetic observed_as_* edges are not supported".into(),
        ));
    }

    // Taxonomy-sensitive property names apply only to their matching binding kind.
    let mut var_kinds: std::collections::HashMap<String, VarKind> =
        std::collections::HashMap::new();
    for element in query.patterns().flat_map(|pattern| &pattern.elements) {
        match element {
            PatternElement::Node(n) => {
                if let Some(v) = n.variable.as_deref() {
                    var_kinds.insert(v.to_owned(), VarKind::Node);
                }
            }
            PatternElement::Edge(e) => {
                if let Some(v) = e.variable.as_deref() {
                    let kind = if e
                        .relations
                        .iter()
                        .any(|r| SYNTHETIC_RELATIONS.contains(&r.as_str()))
                    {
                        VarKind::SyntheticEdge
                    } else {
                        VarKind::Edge
                    };
                    var_kinds.insert(v.to_owned(), kind);
                }
            }
        }
    }

    validate_absence_expr(&mut query.where_clause, &var_kinds)?;

    let mut validate_err: Option<QueryError> = None;
    query.where_clause.for_each_condition_mut(&mut |cond| {
        if validate_err.is_some() {
            return;
        }
        let kind = var_kinds
            .get(cond.variable.as_str())
            .copied()
            .unwrap_or(VarKind::Node);
        if let PropertyRef::MetadataPath(_) = &cond.property {
            if kind != VarKind::Edge {
                validate_err = Some(QueryError::Validation(
                    "metadata paths require a canonical edge binding".into(),
                ));
                return;
            }
        }
        let is_edge = matches!(kind, VarKind::Edge | VarKind::SyntheticEdge);
        if let Err(e) = validate_condition(cond, is_edge) {
            validate_err = Some(e);
        }
    });
    if let Some(e) = validate_err {
        return Err(e);
    }

    for item in &query.order_by {
        let kind = var_kinds.get(item.variable.as_str()).ok_or_else(|| {
            QueryError::Validation(format!("unknown variable '{}' in ORDER BY", item.variable))
        })?;
        match &item.property {
            PropertyRef::Field(field) if field == "namespace" => {
                return Err(QueryError::Validation(
                    "namespace is set by CompileOptions, not query text".into(),
                ));
            }
            PropertyRef::Field(field) if matches!(field.as_str(), "properties" | "metadata") => {
                return Err(QueryError::Validation(
                    "ORDER BY requires a JSON path, not a properties or metadata container".into(),
                ));
            }
            PropertyRef::MetadataPath(_) if *kind != VarKind::Edge => {
                return Err(QueryError::Validation(
                    "metadata paths require a canonical edge binding".into(),
                ));
            }
            _ => {}
        }
    }

    Ok(warnings)
}

/// Validate local path bindings without exposing them to the outer scalar walkers.
fn validate_absence_expr(
    expr: &mut WhereExpr,
    outer: &std::collections::HashMap<String, VarKind>,
) -> Result<(), QueryError> {
    match expr {
        WhereExpr::NotExists(pattern) => validate_absence_pattern(pattern, outer),
        WhereExpr::And(left, right) | WhereExpr::Or(left, right) => {
            validate_absence_expr(left, outer)?;
            validate_absence_expr(right, outer)
        }
        WhereExpr::True | WhereExpr::Condition(_) => Ok(()),
    }
}

fn validate_absence_pattern(
    pattern: &mut MatchPattern,
    outer: &std::collections::HashMap<String, VarKind>,
) -> Result<(), QueryError> {
    if pattern.elements.is_empty() {
        return Err(QueryError::Validation(
            "NOT EXISTS requires a nonempty path".into(),
        ));
    }
    validate_pattern_shape(&pattern.elements)?;
    let mut nodes = HashSet::new();
    let mut edges = HashSet::new();
    let mut correlated = false;
    for element in &mut pattern.elements {
        match element {
            PatternElement::Node(node) => {
                if node.properties.contains_key("namespace") {
                    return Err(QueryError::Validation(
                        "namespace is set by CompileOptions, not query text".into(),
                    ));
                }
                if let Some(var) = node.variable.as_deref() {
                    if edges.contains(var)
                        || outer.get(var).is_some_and(|kind| *kind != VarKind::Node)
                    {
                        return Err(QueryError::Validation(format!(
                            "variable '{var}' cannot bind both a node and an edge in NOT EXISTS"
                        )));
                    }
                    if !nodes.insert(var.to_owned()) {
                        return Err(QueryError::Unsupported(format!(
                            "repeated node variable '{var}' in NOT EXISTS is not supported"
                        )));
                    }
                    correlated |= outer.get(var) == Some(&VarKind::Node);
                }
            }
            PatternElement::Edge(edge) => {
                if let Some(var) = edge.variable.as_deref() {
                    if let Some(kind) = outer.get(var) {
                        if *kind != VarKind::Node {
                            return Err(QueryError::Validation(format!(
                                "edge variable '{var}' is already bound outside; edge variables are not correlated into NOT EXISTS"
                            )));
                        }
                        return Err(QueryError::Validation(format!(
                            "variable '{var}' cannot bind both a node and an edge in NOT EXISTS"
                        )));
                    }
                    if nodes.contains(var) {
                        return Err(QueryError::Validation(format!(
                            "variable '{var}' cannot bind both a node and an edge in NOT EXISTS"
                        )));
                    }
                    if !edges.insert(var.to_owned()) {
                        return Err(QueryError::Unsupported(format!(
                            "repeated edge variable '{var}' in NOT EXISTS is not supported"
                        )));
                    }
                }
                if edge.min_hops != 1 || edge.max_hops != 1 {
                    return Err(QueryError::Unsupported(
                        "NOT EXISTS requires canonical fixed one-hop edges".into(),
                    ));
                }
                for relation in &mut edge.relations {
                    if relation.starts_with("observed_as_") {
                        return Err(QueryError::Unsupported(
                            "synthetic edges are not supported inside NOT EXISTS".into(),
                        ));
                    }
                    let parsed = EdgeRelation::from_str(relation)
                        .map_err(|err| QueryError::Validation(err.to_string()))?;
                    *relation = parsed.as_str().to_owned();
                }
            }
        }
    }
    if !correlated {
        return Err(QueryError::Validation(
            "NOT EXISTS requires a correlated outer node variable".into(),
        ));
    }
    Ok(())
}

/// Preserve within-path binding rules and connect all comma components by named nodes.
fn validate_bindings_and_connectivity(query: &GqlQuery) -> Result<(), QueryError> {
    let mut seen_node_vars: HashSet<&str> = HashSet::new();
    let mut seen_edge_vars: HashSet<&str> = HashSet::new();
    let mut path_vars = Vec::new();
    for (path_index, pattern) in query.patterns().enumerate() {
        if path_index > 0 && pattern.elements.is_empty() {
            return Err(QueryError::Validation(
                "empty additional MATCH pattern".into(),
            ));
        }
        validate_pattern_shape(&pattern.elements)?;
        let mut local_node_vars = HashSet::new();
        for element in &pattern.elements {
            match element {
                PatternElement::Node(node) => {
                    if let Some(var) = node.variable.as_deref() {
                        if seen_edge_vars.contains(var) {
                            return Err(QueryError::Validation(format!(
                                "variable '{var}' cannot bind both a node and an edge"
                            )));
                        }
                        if !local_node_vars.insert(var) {
                            return Err(QueryError::Unsupported(format!(
                                "repeated node variable '{var}' (cycle / self-reachability \
                                 requires alias-equality predicates not yet implemented)"
                            )));
                        }
                        seen_node_vars.insert(var);
                    }
                }
                PatternElement::Edge(edge) => {
                    if let Some(var) = edge.variable.as_deref() {
                        if seen_node_vars.contains(var) {
                            return Err(QueryError::Validation(format!(
                                "variable '{var}' cannot bind both a node and an edge"
                            )));
                        }
                        if !seen_edge_vars.insert(var) {
                            return Err(QueryError::Unsupported(format!(
                                "repeated edge variable '{var}' not supported"
                            )));
                        }
                    }
                }
            }
        }
        path_vars.push(local_node_vars);
    }

    if path_vars.len() > 1 {
        // A later path can bridge earlier components; textual adjacency is not required.
        let mut reached = vec![false; path_vars.len()];
        reached[0] = true;
        let mut names = path_vars[0].clone();
        loop {
            let mut changed = false;
            for (index, vars) in path_vars.iter().enumerate() {
                if !reached[index] && !names.is_disjoint(vars) {
                    reached[index] = true;
                    names.extend(vars.iter().copied());
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        if reached.iter().any(|connected| !connected) {
            return Err(QueryError::Unsupported(
                "comma-separated MATCH patterns must be connected through shared named nodes"
                    .into(),
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VarKind {
    Node,
    Edge,
    SyntheticEdge,
}

fn validate_condition(cond: &mut Condition, is_edge: bool) -> Result<(), QueryError> {
    match &cond.property {
        PropertyRef::Field(property) if property == "namespace" => Err(QueryError::Validation(
            "namespace is set by CompileOptions, not query text".into(),
        )),
        PropertyRef::Field(property) if property == "kind" && !is_edge => Ok(()),
        PropertyRef::Field(property) if property == "relation" && is_edge => {
            let normalize = |s: &mut String| -> Result<(), QueryError> {
                let parsed = EdgeRelation::from_str(s)
                    .map_err(|err| QueryError::Validation(err.to_string()))?;
                *s = parsed.as_str().to_string();
                Ok(())
            };
            if matches!(
                cond.op,
                CompareOp::Contains
                    | CompareOp::StartsWith
                    | CompareOp::IsNotNull
                    | CompareOp::IsNull
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
