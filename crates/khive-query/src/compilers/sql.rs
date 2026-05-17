//! Compile GQL AST to parameterized SQL.
//!
//! Two compilation paths:
//! - Fixed-length patterns (all edges *1..1) → JOIN chain
//! - Variable-length patterns (any edge *N..M where M>1) → recursive CTE
//!
//! Security invariants (MAJ-1/MAJ-2/MAJ-3 from critic review):
//! - Namespace injection: WHERE clause always comes from CompileOptions.scopes, never the query.
//! - Edge property whitelist: only `relation` and `weight` are queryable edge columns.
//! - Depth cap: recursive CTE depth is min(requested, 10).

use crate::ast::*;
use crate::error::QueryError;
use crate::validate::validate;
use khive_storage::types::SqlValue;

#[derive(Debug)]
pub struct CompiledQuery {
    pub sql: String,
    pub params: Vec<SqlValue>,
    pub return_vars: Vec<ReturnItem>,
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
    validate(&mut query)?;

    if query.pattern.has_variable_length() {
        compile_variable_length(&query, opts)
    } else {
        compile_fixed_length(&query, opts)
    }
}

fn namespace_filter(alias: &str, opts: &CompileOptions, params: &mut Vec<SqlValue>) -> String {
    if opts.scopes.is_empty() {
        String::new()
    } else if opts.scopes.len() == 1 {
        params.push(SqlValue::Text(opts.scopes[0].clone()));
        format!(" AND {alias}.namespace = ?{}", params.len())
    } else {
        let placeholders: Vec<String> = opts
            .scopes
            .iter()
            .map(|s| {
                params.push(SqlValue::Text(s.clone()));
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
    let mut params: Vec<SqlValue> = Vec::new();
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
                    params.push(SqlValue::Text(kind.clone()));
                    where_parts.push(format!("{alias}.kind = ?{}", params.len()));
                }

                for (key, val) in &np.properties {
                    params.push(SqlValue::Text(val.clone()));
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

                edge_aliases.push(e_alias.clone());

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

                let next_alias = format!("n{}", node_idx);

                let next_join_col = if target_join == "CASE_BOTH" {
                    format!(
                        "CASE WHEN {e_alias}.source_id = {prev_node}.id THEN {e_alias}.target_id ELSE {e_alias}.source_id END"
                    )
                } else {
                    format!("{e_alias}.{target_join}")
                };

                join_parts.push(format!("JOIN graph_edges {e_alias} ON {source_join}"));

                let ens_filter = namespace_filter(&e_alias, opts, &mut params);
                if !ens_filter.is_empty() {
                    where_parts.push(ens_filter.trim_start_matches(" AND ").to_string());
                }

                join_parts.push(format!(
                    "JOIN entities {next_alias} ON {next_alias}.id = {next_join_col}"
                ));

                if !ep.relations.is_empty() {
                    if ep.relations.len() == 1 {
                        params.push(SqlValue::Text(ep.relations[0].clone()));
                        where_parts.push(format!("{e_alias}.relation = ?{}", params.len()));
                    } else {
                        let placeholders: Vec<String> = ep
                            .relations
                            .iter()
                            .map(|r| {
                                params.push(SqlValue::Text(r.clone()));
                                format!("?{}", params.len())
                            })
                            .collect();
                        where_parts.push(format!(
                            "{e_alias}.relation IN ({})",
                            placeholders.join(", ")
                        ));
                    }
                }

                if let Some(ref var) = ep.variable {
                    var_to_alias.insert(var.clone(), (e_alias.clone(), VarKind::Edge));
                }

                edge_idx += 1;
            }
        }
    }

    // WHERE clause conditions from GQL WHERE
    for cond in &query.where_clause {
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
            VarKind::Edge => {
                // MAJ-1: edge property whitelist — only relation and weight are queryable
                match cond.property.as_str() {
                    "relation" | "weight" => format!("{alias}.{}", cond.property),
                    other => {
                        return Err(QueryError::Validation(format!(
                            "edge property '{other}' not queryable; use 'relation' or 'weight'"
                        )))
                    }
                }
            }
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
                params.push(SqlValue::Text(s.clone()));
                let collate = if matches!(cond.op, CompareOp::Eq | CompareOp::Like) {
                    " COLLATE NOCASE"
                } else {
                    ""
                };
                where_parts.push(format!("{col_expr} {op_str} ?{}{}", params.len(), collate));
            }
            ConditionValue::Number(n) => {
                params.push(SqlValue::Float(*n));
                where_parts.push(format!("{col_expr} {op_str} ?{}", params.len()));
            }
            ConditionValue::Bool(b) => {
                params.push(SqlValue::Integer(if *b { 1 } else { 0 }));
                where_parts.push(format!("{col_expr} {op_str} ?{}", params.len()));
            }
        }
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
                             {alias}.kind AS {var}_kind, {alias}.name AS {var}_name, \
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
    params.push(SqlValue::Integer(limit as i64));

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
    })
}

/// Compile variable-length patterns to a recursive CTE.
///
/// Depth is capped at min(requested, 10) — MAJ-2 (parameterized min_depth, not literal).
fn compile_variable_length(
    query: &GqlQuery,
    opts: &CompileOptions,
) -> Result<CompiledQuery, QueryError> {
    let mut params: Vec<SqlValue> = Vec::new();
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
    let max_depth = edge.max_hops.min(10);
    let min_depth = edge.min_hops;

    // Build start-node conditions
    let mut start_conditions: Vec<String> = vec!["s.deleted_at IS NULL".to_string()];
    let ns_filter = namespace_filter("s", opts, &mut params);
    if !ns_filter.is_empty() {
        start_conditions.push(ns_filter.trim_start_matches(" AND ").to_string());
    }

    if let Some(ref kind) = start.kind {
        params.push(SqlValue::Text(kind.clone()));
        start_conditions.push(format!("s.kind = ?{}", params.len()));
    }
    for (key, val) in &start.properties {
        params.push(SqlValue::Text(val.clone()));
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
            params.push(SqlValue::Text(edge.relations[0].clone()));
            relation_condition = format!(" AND e.relation = ?{}", params.len());
        } else {
            let placeholders: Vec<String> = edge
                .relations
                .iter()
                .map(|r| {
                    params.push(SqlValue::Text(r.clone()));
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

    params.push(SqlValue::Integer(max_depth as i64));
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
        params.push(SqlValue::Text(kind.clone()));
        end_conditions.push(format!("r.kind = ?{}", params.len()));
    }
    for (key, val) in &end.properties {
        params.push(SqlValue::Text(val.clone()));
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

    // WHERE clause conditions
    for cond in &query.where_clause {
        // Map variables to appropriate aliases
        let col_alias = if start.variable.as_deref() == Some(&cond.variable) {
            "s"
        } else if end.variable.as_deref() == Some(&cond.variable) {
            "r"
        } else {
            return Err(QueryError::Compile(format!(
                "variable '{}' in WHERE not supported in variable-length pattern (only start/end node variables)",
                cond.variable
            )));
        };

        let col_expr = if cond.property == "name" || cond.property == "kind" {
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
                params.push(SqlValue::Text(s.clone()));
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
                params.push(SqlValue::Float(*n));
                if col_alias == "s" {
                    start_conditions.push(format!("{col_expr} {op_str} ?{}", params.len()));
                } else {
                    end_conditions.push(format!("{col_expr} {op_str} ?{}", params.len()));
                }
            }
            ConditionValue::Bool(b) => {
                params.push(SqlValue::Integer(if *b { 1 } else { 0 }));
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
        params.push(SqlValue::Integer(min_depth as i64));
        end_conditions.push(format!("t.depth >= ?{}", params.len()));
    }

    let limit = query.limit.unwrap_or(opts.max_limit).min(opts.max_limit);
    params.push(SqlValue::Integer(limit as i64));
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
                                 s.kind AS {var}_kind, s.name AS {var}_name, \
                                 s.properties AS {var}_properties, \
                                 s.created_at AS {var}_created_at, \
                                 s.updated_at AS {var}_updated_at"
                            ));
                        } else {
                            select_parts.push(format!(
                                "r.id AS {var}_id, r.namespace AS {var}_namespace, \
                                 r.kind AS {var}_kind, r.name AS {var}_name, \
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
             JOIN graph_edges e ON {seed_join}{e_ns_filter}{relation_condition} \
             WHERE {start_where} \
             UNION ALL \
             SELECT t.start_id, {recurse_next}, t.depth + 1, \
                    t.path || ',' || {recurse_next}, \
                    t.total_weight + e.weight, \
                    e.id, e.relation, e.weight \
             FROM traverse t \
             JOIN graph_edges e ON {recurse_join}{e_ns_filter}{relation_condition} \
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
            .any(|p| matches!(p, SqlValue::Text(s) if s == "research"));
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
    fn depth_cap_at_ten() {
        // MAJ-2: depth capped at 10 regardless of query request
        let q = gql::parse("MATCH (a)-[:extends*1..50]->(b) RETURN b").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        // The depth parameter must be <= 10
        let depth_val = compiled.params.iter().find_map(|p| {
            if let SqlValue::Integer(n) = p {
                Some(*n)
            } else {
                None
            }
        });
        assert!(depth_val.unwrap() <= 10, "depth must be capped at 10");
    }

    #[test]
    fn limit_capped_by_max_limit() {
        // Query requests 1000, max_limit is 500 — result should be 500
        let q = gql::parse("MATCH (a:concept)-[e]->(b) RETURN a LIMIT 1000").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        let limit_param = compiled.params.last().unwrap();
        assert!(
            matches!(limit_param, SqlValue::Integer(500)),
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
            .any(|p| matches!(p, SqlValue::Text(s) if s == "gizmo"));
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
            .any(|p| matches!(p, SqlValue::Text(s) if s == "paper"));
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
            .any(|p| matches!(p, SqlValue::Text(s) if s == "paper"));
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
}
