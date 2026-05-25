//! Compile GQL AST to parameterized SQL.
//!
//! Two compilation paths:
//! - Fixed-length patterns (all edges *1..1) → JOIN chain
//! - Variable-length patterns (any edge *N..M where M>1) → recursive CTE
//!
//! Synthetic edge paths (ADR-041):
//! - Relations prefixed `observed_as_*` join against `event_observations`, not `graph_edges`.
//!
//! Security invariants (MAJ-1/MAJ-2/MAJ-3 from critic review):
//! - Namespace injection: WHERE clause always comes from CompileOptions.scopes, never the query.
//! - Edge property whitelist: only `relation` and `weight` are queryable edge columns.
//! - Depth cap: recursive CTE depth capped at MAX_DEPTH; exceeding it errors at validation.

use crate::ast::*;
use crate::error::QueryError;
use crate::validate::{validate_with_warnings, MAX_DEPTH};

/// Observation roles used by the synthetic edge compiler (ADR-041 §8).
const SYNTHETIC_RELATIONS: &[&str] = &[
    "observed_as_candidate",
    "observed_as_selected",
    "observed_as_target",
    "observed_as_signal",
];

/// Returns `true` when the relation string is a synthetic ADR-041 observation edge.
fn is_synthetic(rel: &str) -> bool {
    SYNTHETIC_RELATIONS.contains(&rel)
}

/// Returns the `role` value that maps to the given synthetic relation.
fn synthetic_role(rel: &str) -> Option<&'static str> {
    match rel {
        "observed_as_candidate" => Some("candidate"),
        "observed_as_selected" => Some("selected"),
        "observed_as_target" => Some("target"),
        "observed_as_signal" => Some("signal"),
        _ => None,
    }
}

#[derive(Debug)]
pub struct CompiledQuery {
    pub sql: String,
    pub params: Vec<QueryValue>,
    pub return_vars: Vec<ReturnItem>,
    pub warnings: Vec<String>,
}

pub struct CompileOptions {
    /// Namespace scope. Empty = cross-namespace (all). Non-empty = filter to these namespaces.
    pub scopes: Vec<String>,
    /// Hard limit cap (server-side safety). Query limit is min(requested, max_limit).
    pub max_limit: usize,
}

impl Default for CompileOptions {
    fn default() -> Self {
        Self {
            scopes: Vec::new(),
            max_limit: 500,
        }
    }
}

pub fn compile(query: &GqlQuery, opts: &CompileOptions) -> Result<CompiledQuery, QueryError> {
    if query.pattern.elements.is_empty() {
        return Err(QueryError::Compile("empty pattern".into()));
    }

    // Validate edge relations + structural rules before emitting SQL.
    let mut query = query.clone();
    let warnings = validate_with_warnings(&mut query)?;

    let mut compiled = if query.pattern.has_variable_length() {
        compile_variable_length(&query, opts)?
    } else {
        compile_fixed_length(&query, opts)?
    };
    compiled.warnings = warnings;
    Ok(compiled)
}

fn namespace_filter(alias: &str, opts: &CompileOptions, params: &mut Vec<QueryValue>) -> String {
    if opts.scopes.is_empty() {
        String::new()
    } else if opts.scopes.len() == 1 {
        params.push(QueryValue::Text(opts.scopes[0].clone()));
        format!(" AND {alias}.namespace = ?{}", params.len())
    } else {
        let placeholders: Vec<String> = opts
            .scopes
            .iter()
            .map(|s| {
                params.push(QueryValue::Text(s.clone()));
                format!("?{}", params.len())
            })
            .collect();
        format!(" AND {alias}.namespace IN ({})", placeholders.join(", "))
    }
}

/// Compile fixed-length patterns to a chain of JOINs.
///
/// MATCH (a:concept)-[e:introduced_by]->(b:paper) WHERE ... RETURN a, e, b LIMIT 10
/// →
/// SELECT a.*, e.*, b.*
/// FROM entities a
/// JOIN graph_edges e ON e.source_id = a.id
/// JOIN entities b ON b.id = e.target_id
/// WHERE a.kind = 'concept' AND e.relation = 'introduced_by' AND b.kind = 'paper'
///   AND a.deleted_at IS NULL AND b.deleted_at IS NULL
/// LIMIT 10
fn compile_fixed_length(
    query: &GqlQuery,
    opts: &CompileOptions,
) -> Result<CompiledQuery, QueryError> {
    let mut params: Vec<QueryValue> = Vec::new();
    let mut from_parts: Vec<String> = Vec::new();
    let mut join_parts: Vec<String> = Vec::new();
    let mut where_parts: Vec<String> = Vec::new();
    let mut select_parts: Vec<String> = Vec::new();

    let mut node_aliases: Vec<String> = Vec::new();
    let mut edge_aliases: Vec<String> = Vec::new();
    let mut var_to_alias: std::collections::HashMap<String, (String, VarKind)> =
        std::collections::HashMap::new();

    let mut node_idx = 0usize;
    let mut edge_idx = 0usize;

    for element in &query.pattern.elements {
        match element {
            PatternElement::Node(np) => {
                let alias = format!("n{node_idx}");
                node_aliases.push(alias.clone());

                if node_idx == 0 {
                    from_parts.push(format!("entities {alias}"));
                }

                where_parts.push(format!("{alias}.deleted_at IS NULL"));

                let ns_filter = namespace_filter(&alias, opts, &mut params);
                if !ns_filter.is_empty() {
                    where_parts.push(ns_filter.trim_start_matches(" AND ").to_string());
                }

                if let Some(ref kind) = np.kind {
                    params.push(QueryValue::Text(kind.clone()));
                    where_parts.push(format!("{alias}.kind = ?{}", params.len()));
                }

                if let Some(ref et) = np.entity_type {
                    params.push(QueryValue::Text(et.clone()));
                    where_parts.push(format!("{alias}.entity_type = ?{}", params.len()));
                }

                for (key, val) in &np.properties {
                    params.push(QueryValue::Text(val.clone()));
                    if key == "name" {
                        where_parts
                            .push(format!("{alias}.name = ?{} COLLATE NOCASE", params.len()));
                    } else {
                        where_parts.push(format!(
                            "json_extract({alias}.properties, '$.{}') = ?{} COLLATE NOCASE",
                            key.replace('\'', "''"),
                            params.len()
                        ));
                    }
                }

                if let Some(ref var) = np.variable {
                    var_to_alias.insert(var.clone(), (alias.clone(), VarKind::Node));
                }

                node_idx += 1;
            }
            PatternElement::Edge(ep) => {
                let e_alias = format!("e{edge_idx}");
                let prev_node = &node_aliases[node_aliases.len() - 1];
                let next_alias = format!("n{}", node_idx);

                edge_aliases.push(e_alias.clone());

                // Detect synthetic event_observations edges (ADR-041 §8).
                // A synthetic edge is one whose only relation(s) are observed_as_* names.
                // Mixed synthetic+canonical relations are rejected: the two tables don't share
                // a common join key that would make an OR across them meaningful.
                let has_synthetic = ep.relations.iter().any(|r| is_synthetic(r));
                let has_canonical = ep.relations.iter().any(|r| !is_synthetic(r));
                if has_synthetic && has_canonical {
                    return Err(QueryError::Compile(
                        "cannot mix synthetic observed_as_* relations with canonical edge relations \
                         in a single edge pattern"
                            .into(),
                    ));
                }

                if has_synthetic {
                    // Synthetic edge: join event_observations.
                    // Direction is always event → entity/note (OUT from the event node).
                    // The event node is the source (prev_node); the entity/note is the target.
                    if !matches!(ep.direction, EdgeDirection::Out) {
                        return Err(QueryError::Compile(
                            "synthetic observed_as_* edges are always event → entity (outbound only)".into(),
                        ));
                    }
                    join_parts.push(format!(
                        "JOIN event_observations {e_alias} ON {e_alias}.event_id = {prev_node}.id"
                    ));
                    // Roles: collect the unique role values from the synthetic relation names.
                    let roles: Vec<&'static str> = ep
                        .relations
                        .iter()
                        .filter_map(|r| synthetic_role(r))
                        .collect();
                    if roles.len() == 1 {
                        params.push(QueryValue::Text(roles[0].to_string()));
                        where_parts.push(format!("{e_alias}.role = ?{}", params.len()));
                    } else if roles.len() > 1 {
                        let placeholders: Vec<String> = roles
                            .iter()
                            .map(|r| {
                                params.push(QueryValue::Text(r.to_string()));
                                format!("?{}", params.len())
                            })
                            .collect();
                        where_parts
                            .push(format!("{e_alias}.role IN ({})", placeholders.join(", ")));
                    }
                    // Join the target node via event_observations.entity_id.
                    join_parts.push(format!(
                        "JOIN entities {next_alias} ON {next_alias}.id = {e_alias}.entity_id"
                    ));
                } else {
                    // Standard canonical edge: join graph_edges.
                    let (source_join, target_join) = match ep.direction {
                        EdgeDirection::Out => (
                            format!("{e_alias}.source_id = {prev_node}.id"),
                            "target_id",
                        ),
                        EdgeDirection::In => (
                            format!("{e_alias}.target_id = {prev_node}.id"),
                            "source_id",
                        ),
                        EdgeDirection::Both => (
                            format!(
                                "({e_alias}.source_id = {prev_node}.id OR {e_alias}.target_id = {prev_node}.id)"
                            ),
                            "CASE_BOTH",
                        ),
                    };

                    let next_join_col = if target_join == "CASE_BOTH" {
                        format!(
                            "CASE WHEN {e_alias}.source_id = {prev_node}.id THEN {e_alias}.target_id ELSE {e_alias}.source_id END"
                        )
                    } else {
                        format!("{e_alias}.{target_join}")
                    };

                    join_parts.push(format!(
                        "JOIN graph_edges {e_alias} ON {source_join} AND {e_alias}.deleted_at IS NULL"
                    ));

                    let ens_filter = namespace_filter(&e_alias, opts, &mut params);
                    if !ens_filter.is_empty() {
                        where_parts.push(ens_filter.trim_start_matches(" AND ").to_string());
                    }

                    join_parts.push(format!(
                        "JOIN entities {next_alias} ON {next_alias}.id = {next_join_col}"
                    ));

                    if !ep.relations.is_empty() {
                        if ep.relations.len() == 1 {
                            params.push(QueryValue::Text(ep.relations[0].clone()));
                            where_parts.push(format!("{e_alias}.relation = ?{}", params.len()));
                        } else {
                            let placeholders: Vec<String> = ep
                                .relations
                                .iter()
                                .map(|r| {
                                    params.push(QueryValue::Text(r.clone()));
                                    format!("?{}", params.len())
                                })
                                .collect();
                            where_parts.push(format!(
                                "{e_alias}.relation IN ({})",
                                placeholders.join(", ")
                            ));
                        }
                    }
                }

                if let Some(ref var) = ep.variable {
                    var_to_alias.insert(var.clone(), (e_alias.clone(), VarKind::Edge));
                }

                edge_idx += 1;
            }
        }
    }

    // WHERE clause conditions from GQL WHERE (supports AND / OR tree — ADR-008)
    if let Some(where_sql) = compile_where_expr(&query.where_clause, &var_to_alias, &mut params)? {
        where_parts.push(where_sql);
    }

    // SELECT clause
    for item in &query.return_items {
        let var = item.variable();
        if let Some((alias, kind)) = var_to_alias.get(var) {
            match item {
                ReturnItem::Property(_, prop) => {
                    let col = property_to_column(prop, kind)?;
                    select_parts.push(format!("{alias}.{col} AS {var}_{prop}"));
                }
                ReturnItem::Variable(_) => match kind {
                    VarKind::Node => {
                        select_parts.push(format!(
                            "{alias}.id AS {var}_id, {alias}.namespace AS {var}_namespace, \
                             {alias}.kind AS {var}_kind, {alias}.entity_type AS {var}_entity_type, \
                             {alias}.name AS {var}_name, \
                             {alias}.properties AS {var}_properties, \
                             {alias}.created_at AS {var}_created_at, \
                             {alias}.updated_at AS {var}_updated_at"
                        ));
                    }
                    VarKind::Edge => {
                        select_parts.push(format!(
                            "{alias}.id AS {var}_id, {alias}.source_id AS {var}_source, \
                             {alias}.target_id AS {var}_target, \
                             {alias}.relation AS {var}_relation, \
                             {alias}.weight AS {var}_weight"
                        ));
                    }
                },
            }
        } else {
            return Err(QueryError::Compile(format!(
                "unknown variable '{var}' in RETURN clause"
            )));
        }
    }

    let limit = query.limit.unwrap_or(opts.max_limit).min(opts.max_limit);
    params.push(QueryValue::Integer(limit as i64));

    let sql = format!(
        "SELECT {} FROM {} {} WHERE {} LIMIT ?{}",
        select_parts.join(", "),
        from_parts.join(", "),
        join_parts.join(" "),
        where_parts.join(" AND "),
        params.len(),
    );

    Ok(CompiledQuery {
        sql,
        params,
        return_vars: query.return_items.clone(),
        warnings: Vec::new(),
    })
}

/// Compile a `WhereExpr` tree into a SQL fragment, pushing bound parameters into `params`.
///
/// Returns `Ok(None)` for `WhereExpr::True` (no fragment needed), or `Ok(Some(sql))` otherwise.
/// The caller is responsible for wrapping the result in an AND with the structural predicates.
fn compile_where_expr(
    expr: &WhereExpr,
    var_to_alias: &std::collections::HashMap<String, (String, VarKind)>,
    params: &mut Vec<QueryValue>,
) -> Result<Option<String>, QueryError> {
    match expr {
        WhereExpr::True => Ok(None),
        WhereExpr::Condition(cond) => {
            let sql = compile_single_condition(cond, var_to_alias, params)?;
            Ok(Some(sql))
        }
        WhereExpr::And(l, r) => {
            let ls = compile_where_expr(l, var_to_alias, params)?;
            let rs = compile_where_expr(r, var_to_alias, params)?;
            Ok(match (ls, rs) {
                (None, None) => None,
                (Some(s), None) | (None, Some(s)) => Some(s),
                (Some(l), Some(r)) => Some(format!("{l} AND {r}")),
            })
        }
        WhereExpr::Or(l, r) => {
            let ls = compile_where_expr(l, var_to_alias, params)?;
            let rs = compile_where_expr(r, var_to_alias, params)?;
            Ok(match (ls, rs) {
                (None, None) => None,
                (Some(s), None) | (None, Some(s)) => Some(s),
                (Some(l), Some(r)) => Some(format!("({l} OR {r})")),
            })
        }
    }
}

/// Compile a single leaf condition to a SQL predicate string.
fn compile_single_condition(
    cond: &Condition,
    var_to_alias: &std::collections::HashMap<String, (String, VarKind)>,
    params: &mut Vec<QueryValue>,
) -> Result<String, QueryError> {
    let (alias, kind) = var_to_alias.get(&cond.variable).ok_or_else(|| {
        QueryError::Compile(format!(
            "unknown variable '{}' in WHERE clause",
            cond.variable
        ))
    })?;

    let col_expr = match kind {
        VarKind::Node => {
            if cond.property == "name"
                || cond.property == "kind"
                || cond.property == "entity_type"
                || cond.property == "namespace"
            {
                format!("{alias}.{}", cond.property)
            } else {
                format!(
                    "json_extract({alias}.properties, '$.{}')",
                    cond.property.replace('\'', "''")
                )
            }
        }
        VarKind::Edge => match cond.property.as_str() {
            "relation" | "weight" => format!("{alias}.{}", cond.property),
            other => {
                return Err(QueryError::Validation(format!(
                    "edge property '{other}' not queryable; use 'relation' or 'weight'"
                )))
            }
        },
    };

    let op_str = match cond.op {
        CompareOp::Eq => "=",
        CompareOp::Neq => "!=",
        CompareOp::Gt => ">",
        CompareOp::Lt => "<",
        CompareOp::Gte => ">=",
        CompareOp::Lte => "<=",
        CompareOp::Like => "LIKE",
    };

    let sql = match &cond.value {
        ConditionValue::String(s) => {
            params.push(QueryValue::Text(s.clone()));
            let collate = if matches!(cond.op, CompareOp::Eq | CompareOp::Like) {
                " COLLATE NOCASE"
            } else {
                ""
            };
            format!("{col_expr} {op_str} ?{}{}", params.len(), collate)
        }
        ConditionValue::Number(n) => {
            params.push(QueryValue::Float(*n));
            format!("{col_expr} {op_str} ?{}", params.len())
        }
        ConditionValue::Bool(b) => {
            params.push(QueryValue::Integer(if *b { 1 } else { 0 }));
            format!("{col_expr} {op_str} ?{}", params.len())
        }
    };
    Ok(sql)
}

/// Compile variable-length patterns to a recursive CTE.
///
/// Depth is capped at min(requested, 10) — MAJ-2 (parameterized min_depth, not literal).
fn compile_variable_length(
    query: &GqlQuery,
    opts: &CompileOptions,
) -> Result<CompiledQuery, QueryError> {
    let mut params: Vec<QueryValue> = Vec::new();
    let mut var_to_alias: std::collections::HashMap<String, (String, VarKind)> =
        std::collections::HashMap::new();

    // For variable-length, we expect exactly: start_node -[*N..M]-> end_node.
    // Mixed fixed+variable chains and additional trailing pattern elements are
    // not yet supported — reject explicitly rather than silently dropping them.
    let nodes: Vec<&NodePattern> = query.pattern.nodes().collect();
    let edges: Vec<&EdgePattern> = query.pattern.edges().collect();

    if nodes.len() != 2 || edges.len() != 1 || query.pattern.elements.len() != 3 {
        return Err(QueryError::Unsupported(
            "variable-length patterns must be a single start_node -[*N..M]-> end_node \
             (mixed fixed/variable chains are not yet implemented)"
                .into(),
        ));
    }

    let start = &nodes[0];
    let edge = &edges[0];
    let end = &nodes[1];

    // MAJ-2: depth cap — always parameterized, never injected as literal
    let max_depth = edge.max_hops.min(MAX_DEPTH);
    let min_depth = edge.min_hops;

    // Build start-node conditions
    let mut start_conditions: Vec<String> = vec!["s.deleted_at IS NULL".to_string()];
    let ns_filter = namespace_filter("s", opts, &mut params);
    if !ns_filter.is_empty() {
        start_conditions.push(ns_filter.trim_start_matches(" AND ").to_string());
    }

    if let Some(ref kind) = start.kind {
        params.push(QueryValue::Text(kind.clone()));
        start_conditions.push(format!("s.kind = ?{}", params.len()));
    }
    if let Some(ref et) = start.entity_type {
        params.push(QueryValue::Text(et.clone()));
        start_conditions.push(format!("s.entity_type = ?{}", params.len()));
    }
    for (key, val) in &start.properties {
        params.push(QueryValue::Text(val.clone()));
        if key == "name" {
            start_conditions.push(format!("s.name = ?{} COLLATE NOCASE", params.len()));
        } else {
            start_conditions.push(format!(
                "json_extract(s.properties, '$.{}') = ?{} COLLATE NOCASE",
                key.replace('\'', "''"),
                params.len()
            ));
        }
    }

    // Relation filter
    let mut relation_condition = String::new();
    if !edge.relations.is_empty() {
        if edge.relations.len() == 1 {
            params.push(QueryValue::Text(edge.relations[0].clone()));
            relation_condition = format!(" AND e.relation = ?{}", params.len());
        } else {
            let placeholders: Vec<String> = edge
                .relations
                .iter()
                .map(|r| {
                    params.push(QueryValue::Text(r.clone()));
                    format!("?{}", params.len())
                })
                .collect();
            relation_condition = format!(" AND e.relation IN ({})", placeholders.join(", "));
        }
    }

    // Edge namespace filter
    let e_ns_filter = namespace_filter("e", opts, &mut params);

    // Direction-dependent JOIN
    let (seed_join, seed_next, recurse_join, recurse_next) = match edge.direction {
        EdgeDirection::Out => (
            "e.source_id = s.id",
            "e.target_id",
            "e.source_id = t.current_id",
            "e.target_id",
        ),
        EdgeDirection::In => (
            "e.target_id = s.id",
            "e.source_id",
            "e.target_id = t.current_id",
            "e.source_id",
        ),
        EdgeDirection::Both => (
            "(e.source_id = s.id OR e.target_id = s.id)",
            "CASE WHEN e.source_id = s.id THEN e.target_id ELSE e.source_id END",
            "(e.source_id = t.current_id OR e.target_id = t.current_id)",
            "CASE WHEN e.source_id = t.current_id THEN e.target_id ELSE e.source_id END",
        ),
    };

    params.push(QueryValue::Integer(max_depth as i64));
    let depth_param = params.len();

    // End-node conditions (applied in outer WHERE). `r` is always joined
    // unconditionally below so these references resolve regardless of whether
    // the end variable is projected.
    let mut end_conditions: Vec<String> = vec!["r.deleted_at IS NULL".to_string()];
    let r_ns_filter = namespace_filter("r", opts, &mut params);
    if !r_ns_filter.is_empty() {
        end_conditions.push(r_ns_filter.trim_start_matches(" AND ").to_string());
    }
    if let Some(ref kind) = end.kind {
        params.push(QueryValue::Text(kind.clone()));
        end_conditions.push(format!("r.kind = ?{}", params.len()));
    }
    if let Some(ref et) = end.entity_type {
        params.push(QueryValue::Text(et.clone()));
        end_conditions.push(format!("r.entity_type = ?{}", params.len()));
    }
    for (key, val) in &end.properties {
        params.push(QueryValue::Text(val.clone()));
        if key == "name" {
            end_conditions.push(format!("r.name = ?{} COLLATE NOCASE", params.len()));
        } else {
            end_conditions.push(format!(
                "json_extract(r.properties, '$.{}') = ?{} COLLATE NOCASE",
                key.replace('\'', "''"),
                params.len()
            ));
        }
    }

    // WHERE clause conditions for variable-length patterns.
    // Each leaf condition is routed to start_conditions (alias s) or end_conditions
    // (alias r) based on which variable it references.  OR expressions that span
    // both start and end nodes are not yet supported — reject explicitly.
    for cond in query.where_clause.conditions() {
        let col_alias = if start.variable.as_deref() == Some(cond.variable.as_str()) {
            "s"
        } else if end.variable.as_deref() == Some(cond.variable.as_str()) {
            "r"
        } else {
            return Err(QueryError::Compile(format!(
                "variable '{}' in WHERE not supported in variable-length pattern (only start/end node variables)",
                cond.variable
            )));
        };

        let col_expr =
            if cond.property == "name" || cond.property == "kind" || cond.property == "entity_type"
            {
                format!("{col_alias}.{}", cond.property)
            } else {
                format!(
                    "json_extract({col_alias}.properties, '$.{}')",
                    cond.property.replace('\'', "''")
                )
            };

        let op_str = match cond.op {
            CompareOp::Eq => "=",
            CompareOp::Neq => "!=",
            CompareOp::Gt => ">",
            CompareOp::Lt => "<",
            CompareOp::Gte => ">=",
            CompareOp::Lte => "<=",
            CompareOp::Like => "LIKE",
        };

        match &cond.value {
            ConditionValue::String(s) => {
                params.push(QueryValue::Text(s.clone()));
                let collate = if matches!(cond.op, CompareOp::Eq | CompareOp::Like) {
                    " COLLATE NOCASE"
                } else {
                    ""
                };
                if col_alias == "s" {
                    start_conditions
                        .push(format!("{col_expr} {op_str} ?{}{collate}", params.len()));
                } else {
                    end_conditions.push(format!("{col_expr} {op_str} ?{}{collate}", params.len()));
                }
            }
            ConditionValue::Number(n) => {
                params.push(QueryValue::Float(*n));
                if col_alias == "s" {
                    start_conditions.push(format!("{col_expr} {op_str} ?{}", params.len()));
                } else {
                    end_conditions.push(format!("{col_expr} {op_str} ?{}", params.len()));
                }
            }
            ConditionValue::Bool(b) => {
                params.push(QueryValue::Integer(if *b { 1 } else { 0 }));
                if col_alias == "s" {
                    start_conditions.push(format!("{col_expr} {op_str} ?{}", params.len()));
                } else {
                    end_conditions.push(format!("{col_expr} {op_str} ?{}", params.len()));
                }
            }
        }
    }

    // MAJ-2: min_depth is always a bound parameter, never a literal
    if min_depth > 0 {
        params.push(QueryValue::Integer(min_depth as i64));
        end_conditions.push(format!("t.depth >= ?{}", params.len()));
    }

    let limit = query.limit.unwrap_or(opts.max_limit).min(opts.max_limit);
    params.push(QueryValue::Integer(limit as i64));
    let limit_param = params.len();

    // Register variables
    if let Some(ref var) = start.variable {
        var_to_alias.insert(var.clone(), ("s".to_string(), VarKind::Node));
    }
    if let Some(ref var) = end.variable {
        var_to_alias.insert(var.clone(), ("r".to_string(), VarKind::Node));
    }
    if let Some(ref var) = edge.variable {
        var_to_alias.insert(var.clone(), ("e".to_string(), VarKind::Edge));
    }

    // Build SELECT based on RETURN items
    let mut select_parts: Vec<String> = Vec::new();
    let mut has_start = false;

    for item in &query.return_items {
        let var = item.variable();
        if let Some((_, kind)) = var_to_alias.get(var) {
            match item {
                ReturnItem::Property(_, prop) => {
                    let is_start = start.variable.as_deref() == Some(var);
                    if *kind == VarKind::Node {
                        let tbl = if is_start { "s" } else { "r" };
                        if is_start {
                            has_start = true;
                        }
                        let col = property_to_column(prop, kind)?;
                        select_parts.push(format!("{tbl}.{col} AS {var}_{prop}"));
                    } else {
                        let col = match prop.as_str() {
                            "id" => "via_edge",
                            "relation" => "via_relation",
                            "weight" => "via_weight",
                            _ => {
                                return Err(QueryError::Compile(format!(
                                    "unknown edge property '{prop}' in RETURN projection. \
                                     Valid: id, source_id, target_id, relation, weight"
                                )));
                            }
                        };
                        select_parts.push(format!("t.{col} AS {var}_{prop}"));
                    }
                }
                ReturnItem::Variable(_) => match kind {
                    VarKind::Node => {
                        if start.variable.as_deref() == Some(var) {
                            has_start = true;
                            select_parts.push(format!(
                                "s.id AS {var}_id, s.namespace AS {var}_namespace, \
                                 s.kind AS {var}_kind, s.entity_type AS {var}_entity_type, \
                                 s.name AS {var}_name, \
                                 s.properties AS {var}_properties, \
                                 s.created_at AS {var}_created_at, \
                                 s.updated_at AS {var}_updated_at"
                            ));
                        } else {
                            select_parts.push(format!(
                                "r.id AS {var}_id, r.namespace AS {var}_namespace, \
                                 r.kind AS {var}_kind, r.entity_type AS {var}_entity_type, \
                                 r.name AS {var}_name, \
                                 r.properties AS {var}_properties, \
                                 r.created_at AS {var}_created_at, \
                                 r.updated_at AS {var}_updated_at"
                            ));
                        }
                    }
                    VarKind::Edge => {
                        select_parts.push(format!(
                            "t.via_edge AS {var}_id, t.via_relation AS {var}_relation, \
                             t.via_weight AS {var}_weight"
                        ));
                    }
                },
            }
        } else {
            return Err(QueryError::Compile(format!(
                "unknown variable '{var}' in RETURN clause"
            )));
        }
    }

    // Always include traversal metadata
    select_parts.push("t.depth AS _depth".to_string());
    select_parts.push("t.total_weight AS _total_weight".to_string());

    // `s` is optional (only joined if the start variable is projected); `r` is
    // always joined because the outer WHERE always references `r.deleted_at`,
    // `r.namespace` (and possibly r.kind / r.properties) regardless of whether
    // it appears in RETURN.
    let join_start = if has_start {
        "JOIN entities s ON s.id = t.start_id"
    } else {
        ""
    };
    let join_end = "JOIN entities r ON r.id = t.current_id";

    let sql = format!(
        "WITH RECURSIVE traverse(start_id, current_id, depth, path, total_weight, via_edge, via_relation, via_weight) AS (\
             SELECT s.id, {seed_next}, 1, s.id || ',' || {seed_next}, e.weight, \
                    e.id, e.relation, e.weight \
             FROM entities s \
             JOIN graph_edges e ON {seed_join} AND e.deleted_at IS NULL{e_ns_filter}{relation_condition} \
             WHERE {start_where} \
             UNION ALL \
             SELECT t.start_id, {recurse_next}, t.depth + 1, \
                    t.path || ',' || {recurse_next}, \
                    t.total_weight + e.weight, \
                    e.id, e.relation, e.weight \
             FROM traverse t \
             JOIN graph_edges e ON {recurse_join} AND e.deleted_at IS NULL{e_ns_filter}{relation_condition} \
             WHERE t.depth < ?{depth_param} \
               AND (',' || t.path || ',') NOT LIKE '%,' || {recurse_next} || ',%' \
         ) \
         SELECT DISTINCT {select_cols} \
         FROM traverse t \
         {join_start} {join_end} \
         WHERE {end_where} \
         ORDER BY t.depth, t.total_weight DESC \
         LIMIT ?{limit_param}",
        seed_next = seed_next,
        seed_join = seed_join,
        e_ns_filter = e_ns_filter,
        relation_condition = relation_condition,
        start_where = start_conditions.join(" AND "),
        recurse_next = recurse_next,
        recurse_join = recurse_join,
        depth_param = depth_param,
        select_cols = select_parts.join(", "),
        join_start = join_start,
        join_end = join_end,
        end_where = end_conditions.join(" AND "),
        limit_param = limit_param,
    );

    Ok(CompiledQuery {
        sql,
        params,
        return_vars: query.return_items.clone(),
        warnings: Vec::new(),
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VarKind {
    Node,
    Edge,
}

const NODE_COLUMNS: &[&str] = &[
    "id",
    "name",
    "kind",
    "entity_type",
    "namespace",
    "description",
    "properties",
    "created_at",
    "updated_at",
];
const EDGE_COLUMNS: &[&str] = &["id", "source_id", "target_id", "relation", "weight"];

fn property_to_column<'a>(prop: &'a str, kind: &VarKind) -> Result<&'a str, QueryError> {
    let valid = match kind {
        VarKind::Node => NODE_COLUMNS,
        VarKind::Edge => EDGE_COLUMNS,
    };
    if valid.contains(&prop) {
        Ok(prop)
    } else {
        let kind_name = match kind {
            VarKind::Node => "node",
            VarKind::Edge => "edge",
        };
        Err(QueryError::Compile(format!(
            "unknown {kind_name} property '{prop}' in RETURN projection. \
             Valid: {}",
            valid.join(", ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsers::gql;

    fn opts() -> CompileOptions {
        CompileOptions::default()
    }

    fn scoped(namespace: &str) -> CompileOptions {
        CompileOptions {
            scopes: vec![namespace.to_string()],
            max_limit: 500,
        }
    }

    #[test]
    fn fixed_length_basic() {
        let q =
            gql::parse("MATCH (a:concept)-[e:introduced_by]->(b:paper) RETURN a, e, b LIMIT 10")
                .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(compiled.sql.contains("JOIN graph_edges"));
        assert!(compiled.sql.contains("LIMIT"));
        assert_eq!(
            compiled.return_vars,
            vec![
                ReturnItem::Variable("a".into()),
                ReturnItem::Variable("e".into()),
                ReturnItem::Variable("b".into()),
            ]
        );
        // No recursive CTE for fixed-length
        assert!(!compiled.sql.contains("WITH RECURSIVE"));
    }

    #[test]
    fn namespace_scoping_injected() {
        // Namespace must come from opts, never from the query
        let q =
            gql::parse("MATCH (a:concept)-[e:introduced_by]->(b:paper) RETURN a LIMIT 5").unwrap();
        let compiled = compile(&q, &scoped("research")).unwrap();
        assert!(compiled.sql.contains("namespace"));
        // The namespace value must appear as a parameter, not a literal in SQL
        let has_ns_param = compiled
            .params
            .iter()
            .any(|p| matches!(p, QueryValue::Text(s) if s == "research"));
        assert!(has_ns_param, "namespace must be a bound parameter");
    }

    #[test]
    fn edge_property_whitelist_rejects_unknown() {
        // MAJ-1: only 'relation' and 'weight' are queryable edge properties
        let q = gql::parse("MATCH (a)-[e:introduced_by]->(b) WHERE e.source_id = 'x' RETURN a")
            .unwrap();
        let result = compile(&q, &opts());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("source_id") || err.contains("not queryable"),
            "error: {err}"
        );
    }

    #[test]
    fn edge_property_relation_allowed() {
        let q = gql::parse("MATCH (a)-[e]->(b) WHERE e.relation = 'extends' RETURN a").unwrap();
        let result = compile(&q, &opts());
        assert!(
            result.is_ok(),
            "relation should be allowed: {:?}",
            result.err()
        );
    }

    #[test]
    fn edge_property_weight_allowed() {
        let q = gql::parse("MATCH (a)-[e]->(b) WHERE e.weight > 0.5 RETURN a").unwrap();
        let result = compile(&q, &opts());
        assert!(
            result.is_ok(),
            "weight should be allowed: {:?}",
            result.err()
        );
    }

    #[test]
    fn variable_length_uses_cte() {
        let q =
            gql::parse("MATCH (a {name: 'LoRA'})-[:extends*1..3]->(b) RETURN b LIMIT 20").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(compiled.sql.contains("WITH RECURSIVE"));
        assert!(compiled.sql.contains("traverse"));
    }

    #[test]
    fn depth_cap_at_ten_rejects_above_max() {
        // ADR-008 §"Depth limits": exceeding MAX_DEPTH is an InvalidInput error at
        // validation time — the compiler never sees a query with depth > 10.
        let q = gql::parse("MATCH (a)-[:extends*1..50]->(b) RETURN b").unwrap();
        let err = compile(&q, &opts()).unwrap_err();
        assert!(
            matches!(err, QueryError::InvalidInput(_)),
            "expected InvalidInput for depth > 10, got {err:?}"
        );
    }

    #[test]
    fn depth_within_cap_compiles() {
        // depth *1..10 is at the cap — must compile successfully.
        let q = gql::parse("MATCH (a)-[:extends*1..10]->(b) RETURN b").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(compiled.sql.contains("WITH RECURSIVE"));
        // The depth parameter must equal 10
        let depth_val = compiled.params.iter().find_map(|p| {
            if let QueryValue::Integer(n) = p {
                Some(*n)
            } else {
                None
            }
        });
        assert_eq!(depth_val, Some(10), "depth param should be 10");
    }

    #[test]
    fn limit_capped_by_max_limit() {
        // Query requests 1000, max_limit is 500 — result should be 500
        let q = gql::parse("MATCH (a:concept)-[e]->(b) RETURN a LIMIT 1000").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        let limit_param = compiled.params.last().unwrap();
        assert!(
            matches!(limit_param, QueryValue::Integer(500)),
            "expected Integer(500), got {limit_param:?}"
        );
    }

    #[test]
    fn compile_rejects_unknown_relation() {
        let q = gql::parse("MATCH (a)-[:not_a_relation]->(b) RETURN a").unwrap();
        let err = compile(&q, &opts()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not_a_relation"), "msg: {msg}");
    }

    #[test]
    fn compile_unknown_kind_passes_through() {
        // Pack-agnostic: any string is accepted as an entity kind at the query layer.
        // Validation is a pack-handler concern.
        let q = gql::parse("MATCH (a:gizmo)-[:extends]->(b) RETURN a").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        let has_gizmo = compiled
            .params
            .iter()
            .any(|p| matches!(p, QueryValue::Text(s) if s == "gizmo"));
        assert!(
            has_gizmo,
            "pack-agnostic: unknown kind must pass through into SQL params"
        );
    }

    #[test]
    fn compile_kind_passes_through_unchanged() {
        // Pack-agnostic: 'paper' is no longer normalized to 'document' at the query layer.
        // The string passes through as-is.
        let q =
            gql::parse("MATCH (a:paper)-[:introduced_by]->(b:concept) RETURN a LIMIT 1").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        let has_paper = compiled
            .params
            .iter()
            .any(|p| matches!(p, QueryValue::Text(s) if s == "paper"));
        assert!(
            has_paper,
            "kind 'paper' must pass through unchanged into SQL params"
        );
    }

    #[test]
    fn compile_rejects_namespace_in_where() {
        let q =
            gql::parse("MATCH (a:concept)-[:extends]->(b) WHERE a.namespace = 'other' RETURN a")
                .unwrap();
        let err = compile(&q, &opts()).unwrap_err();
        assert!(err.to_string().contains("namespace"), "msg: {err}");
    }

    #[test]
    fn compile_rejects_unknown_relation_in_where() {
        let q = gql::parse("MATCH (a)-[e:extends]->(b) WHERE e.relation = 'related_to' RETURN a")
            .unwrap();
        let err = compile(&q, &opts()).unwrap_err();
        assert!(err.to_string().contains("related_to"), "msg: {err}");
    }

    #[test]
    fn compile_kind_in_where_passes_through_unchanged() {
        // Pack-agnostic: kind strings in WHERE conditions pass through as-is.
        let q = gql::parse("MATCH (a)-[:extends]->(b) WHERE a.kind = 'paper' RETURN a").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        let has_paper = compiled
            .params
            .iter()
            .any(|p| matches!(p, QueryValue::Text(s) if s == "paper"));
        assert!(
            has_paper,
            "kind 'paper' must pass through unchanged into SQL params"
        );
    }

    #[test]
    fn variable_length_return_start_only_joins_end_entity() {
        // Even when only the start variable is projected, the outer query
        // references `r.deleted_at` / `r.namespace`, so entities r must be
        // joined unconditionally.
        let q = gql::parse("MATCH (a:concept)-[:extends*1..3]->(b) RETURN a LIMIT 10").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.contains("JOIN entities r"),
            "entities r must always be joined when r.* conditions are emitted; sql: {}",
            compiled.sql
        );
    }

    #[test]
    fn variable_length_trailing_pattern_unsupported() {
        let q = gql::parse("MATCH (a)-[:extends*1..3]->(b)-[:implements]->(c) RETURN b").unwrap();
        let err = compile(&q, &opts()).unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "expected Unsupported, got {err:?}"
        );
    }

    #[test]
    fn variable_length_mixed_chain_unsupported() {
        // Mixed fixed + variable in one chain — has_variable_length() triggers
        // the variable-length path, which must reject because edges.len() > 1.
        let q = gql::parse("MATCH (a)-[:extends]->(b)-[:implements*1..2]->(c) RETURN c").unwrap();
        let err = compile(&q, &opts()).unwrap_err();
        assert!(matches!(err, QueryError::Unsupported(_)), "got {err:?}");
    }

    #[test]
    fn sparql_star_rejected_as_unsupported() {
        use crate::parsers::sparql;
        let err = sparql::parse("SELECT ?a ?b WHERE { ?a :extends* ?b . }").unwrap_err();
        assert!(matches!(err, QueryError::Unsupported(_)), "got {err:?}");
    }

    /// Regression guard for ISSUE #231.
    ///
    /// Verifies the full SPARQL subject→predicate→object direction contract:
    ///   ?a :extends ?b  must compile so that ?a binds `source_id` and ?b binds `target_id`.
    ///
    /// A swap (subject→target_id, object→source_id) would cause a query for
    /// A–extends→B to return rows where B–extends→A, silently returning wrong results.
    #[test]
    fn sparql_subject_object_direction_compiles_outbound() {
        use crate::parsers::sparql;

        let q = sparql::parse("SELECT ?a ?b WHERE { ?a :extends ?b . }").unwrap();
        let compiled = compile(&q, &opts()).unwrap();

        assert!(
            compiled
                .sql
                .contains("JOIN graph_edges e0 ON e0.source_id = n0.id"),
            "SPARQL subject must bind graph_edges.source_id; sql: {}",
            compiled.sql
        );
        assert!(
            compiled
                .sql
                .contains("JOIN entities n1 ON n1.id = e0.target_id"),
            "SPARQL object must bind graph_edges.target_id; sql: {}",
            compiled.sql
        );
        assert!(
            compiled.sql.contains("e0.relation = ?1"),
            "SPARQL predicate must bind graph_edges.relation; sql: {}",
            compiled.sql
        );
    }

    #[test]
    fn return_property_projection_compiles() {
        let q =
            gql::parse("MATCH (a:concept)-[e:extends]->(b:concept) RETURN a.name, b.name LIMIT 5")
                .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        // Node aliases are n0, n1; the SQL uses `alias.col AS var_prop`
        assert!(
            compiled.sql.contains(".name AS a_name"),
            "sql: {}",
            compiled.sql
        );
        assert!(
            compiled.sql.contains(".name AS b_name"),
            "sql: {}",
            compiled.sql
        );
        assert!(
            !compiled.sql.contains("a_kind"),
            "should not emit full node columns"
        );
    }

    #[test]
    fn return_unknown_node_property_rejected() {
        let q = gql::parse("MATCH (a:concept)-[:extends]->(b) RETURN a.domain LIMIT 5").unwrap();
        let err = compile(&q, &opts()).unwrap_err();
        assert!(
            matches!(err, QueryError::Compile(ref msg) if msg.contains("unknown node property 'domain'")),
            "got {err:?}"
        );
    }

    #[test]
    fn return_unknown_edge_property_rejected() {
        let q = gql::parse("MATCH (a)-[e:extends]->(b) RETURN e.label LIMIT 5").unwrap();
        let err = compile(&q, &opts()).unwrap_err();
        assert!(
            matches!(err, QueryError::Compile(ref msg) if msg.contains("unknown edge property 'label'")),
            "got {err:?}"
        );
    }

    #[test]
    fn return_valid_edge_property_compiles() {
        let q =
            gql::parse("MATCH (a)-[e:extends]->(b) RETURN e.relation, e.weight LIMIT 5").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        // Edge alias is e0; SQL: `e0.relation AS e_relation`
        assert!(
            compiled.sql.contains(".relation AS e_relation"),
            "sql: {}",
            compiled.sql
        );
        assert!(
            compiled.sql.contains(".weight AS e_weight"),
            "sql: {}",
            compiled.sql
        );
    }

    #[test]
    fn entity_type_compiles_as_direct_column_not_json_extract() {
        // entity_type in a NodePattern must become `alias.entity_type = ?N` in the WHERE
        // clause — a direct column reference, not json_extract from the properties blob.
        let q = gql::parse("MATCH (n:document {entity_type: 'paper'})-[:extends]->(m) RETURN n")
            .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.contains(".entity_type = ?"),
            "entity_type must compile to a direct column comparison; sql: {}",
            compiled.sql
        );
        assert!(
            !compiled.sql.contains("json_extract"),
            "entity_type must NOT use json_extract; sql: {}",
            compiled.sql
        );
        let has_paper_param = compiled
            .params
            .iter()
            .any(|p| matches!(p, QueryValue::Text(s) if s == "paper"));
        assert!(
            has_paper_param,
            "entity_type value 'paper' must appear as a bound parameter"
        );
    }

    // --- F047: OR support in WHERE clause (ADR-008 §"GQL WHERE expression") ---

    #[test]
    fn where_or_compiles_to_sql_or() {
        let q = gql::parse(
            "MATCH (a:concept)-[e:extends]->(b) WHERE a.name = 'LoRA' OR a.name = 'QLoRA' RETURN a",
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.contains(" OR "),
            "WHERE OR must produce SQL OR; sql: {}",
            compiled.sql
        );
        let has_lora = compiled
            .params
            .iter()
            .any(|p| matches!(p, QueryValue::Text(s) if s == "LoRA"));
        let has_qlora = compiled
            .params
            .iter()
            .any(|p| matches!(p, QueryValue::Text(s) if s == "QLoRA"));
        assert!(has_lora && has_qlora, "both OR values must be bound params");
    }

    #[test]
    fn where_and_or_precedence() {
        // `a AND b OR c` should compile as `(a AND b) OR c`
        let q = gql::parse(
            "MATCH (a:concept)-[e:extends]->(b) WHERE a.name = 'X' AND a.kind = 'concept' OR b.kind = 'project' RETURN a"
        ).unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        // The SQL should contain an OR at the outer level wrapping the AND group
        assert!(
            compiled.sql.contains(" OR "),
            "expected OR in sql; sql: {}",
            compiled.sql
        );
    }

    // --- F218: event_observations synthetic edge support (ADR-041 §8) ---

    #[test]
    fn synthetic_edge_joins_event_observations() {
        let q = gql::parse("MATCH (ev)-[:observed_as_selected]->(m:memory) RETURN ev, m").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.contains("event_observations"),
            "synthetic edge must join event_observations; sql: {}",
            compiled.sql
        );
        assert!(
            !compiled.sql.contains("graph_edges"),
            "synthetic edge must NOT join graph_edges; sql: {}",
            compiled.sql
        );
        let has_role_param = compiled
            .params
            .iter()
            .any(|p| matches!(p, QueryValue::Text(s) if s == "selected"));
        assert!(has_role_param, "role 'selected' must be a bound parameter");
    }

    #[test]
    fn synthetic_edge_candidate_role() {
        let q = gql::parse("MATCH (ev)-[:observed_as_candidate]->(m) RETURN ev, m").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.contains("event_observations"),
            "sql: {}",
            compiled.sql
        );
        let has_candidate = compiled
            .params
            .iter()
            .any(|p| matches!(p, QueryValue::Text(s) if s == "candidate"));
        assert!(has_candidate, "role 'candidate' must be bound");
    }

    #[test]
    fn synthetic_edge_multi_role() {
        // Multiple observed_as_* relations compile to a role IN (...) predicate.
        let q =
            gql::parse("MATCH (ev)-[:observed_as_candidate|observed_as_selected]->(m) RETURN m")
                .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.contains("event_observations"),
            "sql: {}",
            compiled.sql
        );
        assert!(
            compiled.sql.contains("IN"),
            "multi-role must use IN; sql: {}",
            compiled.sql
        );
    }

    #[test]
    fn mixed_synthetic_and_canonical_rejected() {
        let q = gql::parse("MATCH (ev)-[:observed_as_selected|extends]->(m) RETURN m").unwrap();
        let err = compile(&q, &opts()).unwrap_err();
        assert!(
            matches!(err, QueryError::Compile(_)),
            "mixed synthetic+canonical must be rejected; got {err:?}"
        );
    }

    #[test]
    fn synthetic_edge_inbound_rejected() {
        let q = gql::parse("MATCH (m)<-[:observed_as_selected]-(ev) RETURN m").unwrap();
        let err = compile(&q, &opts()).unwrap_err();
        assert!(
            matches!(err, QueryError::Compile(_)),
            "inbound synthetic edge must be rejected; got {err:?}"
        );
    }
}
