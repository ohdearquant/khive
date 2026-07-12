//! Compile GQL AST to parameterized SQL (JOIN chain or recursive CTE).

use crate::ast::*;
use crate::error::QueryError;
use crate::validate::{validate_with_warnings, MAX_DEPTH};

/// Observation roles used by the synthetic edge compiler.
const SYNTHETIC_RELATIONS: &[&str] = &[
    "observed_as_candidate",
    "observed_as_selected",
    "observed_as_target",
    "observed_as_signal",
];

fn is_synthetic(rel: &str) -> bool {
    SYNTHETIC_RELATIONS.contains(&rel)
}

fn synthetic_role(rel: &str) -> Option<&'static str> {
    match rel {
        "observed_as_candidate" => Some("candidate"),
        "observed_as_selected" => Some("selected"),
        "observed_as_target" => Some("target"),
        "observed_as_signal" => Some("signal"),
        _ => None,
    }
}

/// Union of every primary-substrate table (entities, notes, events, graph_edges)
/// that can legally bind to a canonical `-[:relation]->` endpoint (issue #467).
/// Canonical edge endpoints are not entity-only: `annotates` admits note/entity/
/// note/event/edge targets, and same-substrate relations like `supports` admit
/// note-note pairs. This source lets node binding stay substrate-agnostic instead
/// of hard-coding `entities`.
const PRIMARY_NODE_SQL: &str = "\
    SELECT id, namespace, kind, entity_type, name, description, NULL AS content, \
           NULL AS status, NULL AS salience, NULL AS decay_factor, properties, \
           created_at, updated_at, deleted_at, 'entity' AS substrate_kind \
      FROM entities \
    UNION ALL \
    SELECT id, namespace, kind, NULL AS entity_type, name, NULL AS description, \
           content, status, salience, decay_factor, properties, \
           created_at, updated_at, deleted_at, 'note' AS substrate_kind \
      FROM notes \
    UNION ALL \
    SELECT id, namespace, kind, NULL AS entity_type, verb AS name, \
           NULL AS description, NULL AS content, NULL AS status, \
           NULL AS salience, NULL AS decay_factor, payload AS properties, \
           created_at, created_at AS updated_at, NULL AS deleted_at, \
           'event' AS substrate_kind \
      FROM events \
    UNION ALL \
    SELECT id, namespace, relation AS kind, NULL AS entity_type, NULL AS name, \
           NULL AS description, NULL AS content, NULL AS status, \
           NULL AS salience, NULL AS decay_factor, metadata AS properties, \
           created_at, updated_at, deleted_at, 'edge' AS substrate_kind \
      FROM graph_edges";

fn primary_node_source(alias: &str) -> String {
    format!("({PRIMARY_NODE_SQL}) {alias}")
}

/// Union of entity/note tables that can legally bind to a synthetic
/// `observed_as_*` target (issue #468): `observed_as_target` and
/// `observed_as_signal` both admit entity OR note referents (ADR-041
/// Amendment A2). This source lets the target join stay entity-capable
/// instead of hard-coding `notes`.
const OBSERVATION_TARGET_SQL: &str = "\
    SELECT id, namespace, kind, entity_type, name, description, \
           NULL AS content, NULL AS status, NULL AS salience, \
           NULL AS decay_factor, properties, created_at, updated_at, \
           deleted_at, 'entity' AS referent_kind \
      FROM entities \
    UNION ALL \
    SELECT id, namespace, kind, NULL AS entity_type, name, NULL AS description, \
           content, status, salience, decay_factor, properties, \
           created_at, updated_at, deleted_at, 'note' AS referent_kind \
      FROM notes";

fn observation_target_source(alias: &str) -> String {
    format!("({OBSERVATION_TARGET_SQL}) {alias}")
}

/// Parameterized SQL emitted by the compiler, ready for execution by the runtime.
#[derive(Debug)]
pub struct CompiledQuery {
    pub sql: String,
    pub params: Vec<QueryValue>,
    pub return_vars: Vec<ReturnItem>,
    pub warnings: Vec<String>,
    /// Present when the server-side `max_limit` cap is the binding
    /// constraint on this query (no explicit `LIMIT`, or an explicit
    /// `LIMIT` above the cap). The emitted SQL fetches `max_limit + 1`
    /// rows (a sentinel row) so the execution site can detect whether the
    /// true match count exceeded the cap and warn accordingly, instead of
    /// inferring truncation from the requested `LIMIT` alone (issue #777).
    pub truncation_check: Option<TruncationCheck>,
}

/// See [`CompiledQuery::truncation_check`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TruncationCheck {
    /// The server-side row cap; the execution site truncates to this many
    /// rows after stripping the sentinel.
    pub max_limit: usize,
    /// The caller's explicit `LIMIT`, if any, for warning-message purposes.
    pub requested_limit: Option<usize>,
}

/// Runtime options injected by the caller to scope and cap query execution.
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

/// Compute the SQL `LIMIT` to emit and whether the execution site must
/// perform a cap-sentinel check (issue #777).
///
/// When the caller's own `LIMIT` is at or below `max_limit`, the cap never
/// binds: we fetch exactly what was requested and no truncation is
/// possible. When there is no explicit `LIMIT`, or the explicit `LIMIT`
/// exceeds `max_limit`, the cap is the binding constraint — we fetch one
/// extra row (`max_limit + 1`) so the caller can tell, from the actual
/// result set, whether there were more matches than the cap allows. This
/// replaces inferring truncation from the requested `LIMIT` alone, which is
/// both a false positive (`LIMIT 1000` matching only 20 rows) and a false
/// negative (no `LIMIT`, 501+ rows actually match).
fn effective_limit(
    requested_limit: Option<usize>,
    max_limit: usize,
) -> (usize, Option<TruncationCheck>) {
    match requested_limit {
        Some(limit) if limit <= max_limit => (limit, None),
        requested => (
            max_limit.saturating_add(1),
            Some(TruncationCheck {
                max_limit,
                requested_limit: requested,
            }),
        ),
    }
}

/// Compile a `GqlQuery` AST to a parameterized SQL string and bound parameters.
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
    compiled.warnings.extend(warnings);

    // Defense-in-depth: assert the emitted SQL is SELECT-only.
    // The parsers already reject write-shaped input; this guard ensures a future
    // code path cannot accidentally emit a non-SELECT statement through the
    // compiler. Fails closed with an explicit read-only message.
    assert_select_only(&compiled.sql)?;

    Ok(compiled)
}

/// Assert that the emitted SQL starts with SELECT (or WITH for recursive CTEs).
///
/// This is a compiler-level read-only invariant guard. The parsers reject
/// write-shaped input before AST construction, but this check prevents a
/// hypothetical future code path from emitting `INSERT`/`UPDATE`/`DELETE`
/// SQL through the compile path. It is not a security boundary — the SQLite
/// reader connection already enforces read-only at the driver level.
fn assert_select_only(sql: &str) -> Result<(), QueryError> {
    let first = sql.split_whitespace().next().unwrap_or("").to_uppercase();
    if first == "SELECT" || first == "WITH" {
        return Ok(());
    }
    Err(QueryError::Compile(
        "the query verb is read-only; \
         to mutate the graph use: create, update, link, merge, delete"
            .into(),
    ))
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

/// Compile a node label's `kind` predicate against a primary-substrate union
/// source (the `entities`/`notes`/`events`/`graph_edges` union carrying a
/// `substrate_kind` column, aliased `primary_nodes` in variable-length
/// queries and inlined in fixed-length ones).
///
/// A substrate label (`entity`/`note`/`event`/`edge`) names a table, not a
/// stored `kind` value — stored `kind` is always granular (`concept`,
/// `task`, `observation`, ...). This mirrors the substrate/granular split in
/// `resolve_kind_spec` (`khive-pack-kg/src/handlers/common.rs`), which the
/// verb-dispatch surface (`create`/`list`/`search`) already applies. Without
/// it, `kind = 'entity'` is unsatisfiable by construction: no stored row's
/// `kind` column is ever literally `"entity"` (issue #849).
fn kind_filter_predicate(alias: &str, kind: &str, params: &mut Vec<QueryValue>) -> String {
    match kind {
        "entity" | "note" | "event" | "edge" => {
            params.push(QueryValue::Text(kind.to_string()));
            format!("{alias}.substrate_kind = ?{}", params.len())
        }
        _ => {
            params.push(QueryValue::Text(kind.to_string()));
            format!("{alias}.kind = ?{}", params.len())
        }
    }
}

/// Same as [`kind_filter_predicate`], for the entity/note-only observation-
/// target union (`referent_kind` column, issue #468) — only `entity`/`note`
/// are legal substrate labels there.
fn observation_kind_filter_predicate(
    alias: &str,
    kind: &str,
    params: &mut Vec<QueryValue>,
) -> String {
    match kind {
        "entity" | "note" => {
            params.push(QueryValue::Text(kind.to_string()));
            format!("{alias}.referent_kind = ?{}", params.len())
        }
        _ => {
            params.push(QueryValue::Text(kind.to_string()));
            format!("{alias}.kind = ?{}", params.len())
        }
    }
}

/// Compile an inline property-map equality (`{key: value}`) to a parameterized
/// SQL predicate, either against a direct text column (`text_column`, for keys
/// like `name`/`content` that are stored as dedicated columns) or against
/// `json_extract(<alias>.properties, '$.<key>')`.
///
/// String values bind as `TEXT` with `COLLATE NOCASE` (case-insensitive match,
/// matching prior behavior). Integer values bind as `INTEGER`, and Bool values
/// bind as `INTEGER` (0/1) — SQLite's `json_extract` returns JSON
/// numbers/booleans as numeric storage classes, so a numeric literal must
/// compare against a numeric parameter, not a `COLLATE NOCASE` text
/// comparison that can never match (issue #755). Integer literals bind as
/// `QueryValue::Integer` rather than `Float` so values beyond `f64`'s exact
/// 2^53 integer range (and both `i64` bounds) survive round-trip (issue
/// #832). Float values are rejected here if non-finite, matching the
/// `is_finite()` invariant enforced by WHERE-clause compilation
/// (see `docs/design.md`).
fn compile_property_equality(
    alias: &str,
    key: &str,
    value: &ConditionValue,
    text_column: Option<&str>,
    params: &mut Vec<QueryValue>,
) -> Result<String, QueryError> {
    let is_string = matches!(value, ConditionValue::String(_));
    match value {
        ConditionValue::String(s) => params.push(QueryValue::Text(s.clone())),
        ConditionValue::Integer(n) => params.push(QueryValue::Integer(*n)),
        ConditionValue::Number(n) => {
            if !n.is_finite() {
                return Err(QueryError::InvalidInput(
                    "non-finite float (NaN or Infinity) is not a valid query parameter".into(),
                ));
            }
            params.push(QueryValue::Float(*n));
        }
        ConditionValue::Bool(b) => params.push(QueryValue::Integer(if *b { 1 } else { 0 })),
        ConditionValue::List(_) | ConditionValue::Null => {
            return Err(QueryError::Validation(
                "list and null operands are not valid in inline property maps".into(),
            ));
        }
    }
    let collate = if is_string { " COLLATE NOCASE" } else { "" };
    Ok(match text_column {
        Some(col) => format!("{alias}.{col} = ?{}{collate}", params.len()),
        None => format!(
            "json_extract({alias}.properties, '$.{}') = ?{}{collate}",
            key.replace('\'', "''"),
            params.len()
        ),
    })
}

/// Returns `(source_indices, target_indices)` for synthetic `observed_as_*` edge endpoints.
fn synthetic_endpoint_node_indices(
    elements: &[PatternElement],
) -> (
    std::collections::HashSet<usize>,
    std::collections::HashSet<usize>,
) {
    let mut source_set = std::collections::HashSet::new();
    let mut target_set = std::collections::HashSet::new();
    let mut node_idx = 0usize;
    let mut prev_node_idx: Option<usize> = None;
    for element in elements {
        match element {
            PatternElement::Node(_) => {
                prev_node_idx = Some(node_idx);
                node_idx += 1;
            }
            PatternElement::Edge(ep) => {
                let has_synthetic = ep.relations.iter().any(|r| is_synthetic(r));
                if has_synthetic {
                    if let Some(src_idx) = prev_node_idx {
                        source_set.insert(src_idx);
                        // The target is the next node (current node_idx).
                        target_set.insert(node_idx);
                    }
                }
            }
        }
    }
    (source_set, target_set)
}

/// Compile fixed-length patterns to a JOIN chain.
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

    // Pre-compute which node indices are endpoints of synthetic edges.
    // Source nodes bind to `events`; target nodes bind to the entity/note
    // observation-target union (issue #468).
    let (event_source_indices, observation_target_indices) =
        synthetic_endpoint_node_indices(&query.pattern.elements);

    let mut node_idx = 0usize;
    let mut edge_idx = 0usize;

    for element in &query.pattern.elements {
        match element {
            PatternElement::Node(np) => {
                let alias = format!("n{node_idx}");
                node_aliases.push(alias.clone());

                let is_event_source = event_source_indices.contains(&node_idx);
                let is_observation_target = observation_target_indices.contains(&node_idx);

                if node_idx == 0 {
                    if is_event_source {
                        from_parts.push(format!("events {alias}"));
                    } else if !is_observation_target {
                        // Observation targets are joined by the synthetic edge handler, not FROM.
                        from_parts.push(primary_node_source(&alias));
                    }
                }

                if is_event_source {
                    // Events table does not have `deleted_at`; filter is omitted.
                    // Namespace filter uses the `events.namespace` column directly.
                    let ns_filter = namespace_filter(&alias, opts, &mut params);
                    if !ns_filter.is_empty() {
                        where_parts.push(ns_filter.trim_start_matches(" AND ").to_string());
                    }
                    // `kind` on an event node filters events.kind (e.g. "recall_executed").
                    // The bare substrate label "event" needs no filter here — the FROM
                    // source is already the events table exclusively (issue #849).
                    if let Some(ref kind) = np.kind {
                        if kind != "event" {
                            params.push(QueryValue::Text(kind.clone()));
                            where_parts.push(format!("{alias}.kind = ?{}", params.len()));
                        }
                    }
                    // entity_type and properties are not columns on events — reject explicitly.
                    if np.entity_type.is_some() {
                        return Err(QueryError::Compile(
                            "event nodes do not have an entity_type column".into(),
                        ));
                    }
                    if !np.properties.is_empty() {
                        return Err(QueryError::Compile(
                            "event nodes do not support inline property filters; \
                             use a WHERE clause on verb, outcome, or payload fields"
                                .into(),
                        ));
                    }
                } else if is_observation_target {
                    // Observation targets: entity/note union (joined by the synthetic edge handler).
                    where_parts.push(format!("{alias}.deleted_at IS NULL"));

                    let ns_filter = namespace_filter(&alias, opts, &mut params);
                    if !ns_filter.is_empty() {
                        where_parts.push(ns_filter.trim_start_matches(" AND ").to_string());
                    }

                    if let Some(ref kind) = np.kind {
                        where_parts.push(observation_kind_filter_predicate(
                            &alias,
                            kind,
                            &mut params,
                        ));
                    }

                    // entity_type is NULL on note rows and populated on entity rows;
                    // the filter still applies correctly across the union.
                    if let Some(ref et) = np.entity_type {
                        params.push(QueryValue::Text(et.clone()));
                        where_parts.push(format!("{alias}.entity_type = ?{}", params.len()));
                    }

                    let mut props: Vec<_> = np.properties.iter().collect();
                    props.sort_by_key(|(k, _)| k.as_str());
                    for (key, val) in props {
                        let text_column = if key == "name" || key == "content" {
                            Some(key.as_str())
                        } else {
                            None
                        };
                        where_parts.push(compile_property_equality(
                            &alias,
                            key,
                            val,
                            text_column,
                            &mut params,
                        )?);
                    }
                } else {
                    where_parts.push(format!("{alias}.deleted_at IS NULL"));

                    let ns_filter = namespace_filter(&alias, opts, &mut params);
                    if !ns_filter.is_empty() {
                        where_parts.push(ns_filter.trim_start_matches(" AND ").to_string());
                    }

                    if let Some(ref kind) = np.kind {
                        where_parts.push(kind_filter_predicate(&alias, kind, &mut params));
                    }

                    if let Some(ref et) = np.entity_type {
                        params.push(QueryValue::Text(et.clone()));
                        where_parts.push(format!("{alias}.entity_type = ?{}", params.len()));
                    }

                    let mut props: Vec<_> = np.properties.iter().collect();
                    props.sort_by_key(|(k, _)| k.as_str());
                    for (key, val) in props {
                        let text_column = if key == "name" { Some("name") } else { None };
                        where_parts.push(compile_property_equality(
                            &alias,
                            key,
                            val,
                            text_column,
                            &mut params,
                        )?);
                    }
                }

                if let Some(ref var) = np.variable {
                    let kind = if is_event_source {
                        VarKind::EventNode
                    } else if is_observation_target {
                        VarKind::ObservationTargetNode
                    } else {
                        VarKind::Node
                    };
                    var_to_alias.insert(var.clone(), (alias.clone(), kind));
                }

                node_idx += 1;
            }
            PatternElement::Edge(ep) => {
                let e_alias = format!("e{edge_idx}");
                let prev_node = &node_aliases[node_aliases.len() - 1];
                let next_alias = format!("n{}", node_idx);

                edge_aliases.push(e_alias.clone());

                // Detect synthetic event_observations edges (observed_as_* relations).
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
                    // Join the target node via event_observations.entity_id against the
                    // entity/note referent union, discriminated by referent_kind so
                    // cross-substrate ID collisions cannot occur.
                    join_parts.push(format!(
                        "JOIN {} ON {next_alias}.id = {e_alias}.entity_id \
                         AND {next_alias}.referent_kind = {e_alias}.referent_kind",
                        observation_target_source(&next_alias)
                    ));
                    // Admit exactly the in-code legal (role, referent_kind) pairs:
                    // candidate/selected -> note only; target -> entity or note;
                    // signal -> entity or note (ADR-041 permits both substrates
                    // as brain.feedback targets).
                    where_parts.push(format!(
                        "(({e_alias}.role IN ('candidate', 'selected') AND {e_alias}.referent_kind = 'note') \
                          OR ({e_alias}.role = 'target' AND {e_alias}.referent_kind IN ('entity', 'note')) \
                          OR ({e_alias}.role = 'signal' AND {e_alias}.referent_kind IN ('entity', 'note')))"
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
                        "JOIN {} ON {next_alias}.id = {next_join_col}",
                        primary_node_source(&next_alias)
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

    // WHERE clause conditions from GQL WHERE (supports AND / OR tree)
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
                    VarKind::ObservationTargetNode => {
                        select_parts.push(format!(
                            "{alias}.id AS {var}_id, {alias}.namespace AS {var}_namespace, \
                             {alias}.kind AS {var}_kind, {alias}.entity_type AS {var}_entity_type, \
                             {alias}.status AS {var}_status, \
                             {alias}.content AS {var}_content, \
                             {alias}.salience AS {var}_salience, \
                             {alias}.properties AS {var}_properties, \
                             {alias}.created_at AS {var}_created_at, \
                             {alias}.updated_at AS {var}_updated_at, \
                             {alias}.referent_kind AS {var}_referent_kind"
                        ));
                    }
                    VarKind::EventNode => {
                        select_parts.push(format!(
                            "{alias}.id AS {var}_id, {alias}.namespace AS {var}_namespace, \
                             {alias}.verb AS {var}_verb, {alias}.substrate AS {var}_substrate, \
                             {alias}.actor AS {var}_actor, {alias}.kind AS {var}_kind, \
                             {alias}.outcome AS {var}_outcome, \
                             {alias}.payload AS {var}_payload, \
                             {alias}.created_at AS {var}_created_at"
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

    let (limit, truncation_check) = effective_limit(query.limit, opts.max_limit);
    let limit_i64 = i64::try_from(limit)
        .map_err(|_| QueryError::InvalidInput("limit exceeds i64::MAX".into()))?;
    params.push(QueryValue::Integer(limit_i64));

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
        truncation_check,
    })
}

/// Compile a `WhereExpr` tree into a SQL fragment.
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
        VarKind::ObservationTargetNode => {
            if OBSERVATION_TARGET_COLUMNS.contains(&cond.property.as_str()) {
                format!("{alias}.{}", cond.property)
            } else {
                format!(
                    "json_extract({alias}.properties, '$.{}')",
                    cond.property.replace('\'', "''")
                )
            }
        }
        VarKind::EventNode => {
            // Events table has direct columns only; reject unknown fields.
            if EVENT_COLUMNS.contains(&cond.property.as_str()) {
                format!("{alias}.{}", cond.property)
            } else {
                return Err(QueryError::Validation(format!(
                    "event property '{}' not queryable; valid columns: {}",
                    cond.property,
                    EVENT_COLUMNS.join(", ")
                )));
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

    compile_condition_predicate(&col_expr, cond, params)
}

fn bind_condition_value(
    value: &ConditionValue,
    params: &mut Vec<QueryValue>,
) -> Result<usize, QueryError> {
    match value {
        ConditionValue::String(s) => params.push(QueryValue::Text(s.clone())),
        ConditionValue::Integer(n) => params.push(QueryValue::Integer(*n)),
        ConditionValue::Number(n) => {
            if !n.is_finite() {
                return Err(QueryError::InvalidInput(
                    "non-finite float (NaN or Infinity) is not a valid query parameter".into(),
                ));
            }
            params.push(QueryValue::Float(*n));
        }
        ConditionValue::Bool(b) => {
            params.push(QueryValue::Integer(if *b { 1 } else { 0 }));
        }
        ConditionValue::List(_) | ConditionValue::Null => {
            return Err(QueryError::Validation(
                "operator requires a scalar value".into(),
            ));
        }
    }
    Ok(params.len())
}

fn escape_like_literal(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

fn compile_condition_predicate(
    col_expr: &str,
    cond: &Condition,
    params: &mut Vec<QueryValue>,
) -> Result<String, QueryError> {
    match cond.op {
        CompareOp::Contains | CompareOp::StartsWith => {
            let ConditionValue::String(value) = &cond.value else {
                return Err(QueryError::Validation(
                    "CONTAINS and STARTS WITH require a string literal".into(),
                ));
            };
            let escaped = escape_like_literal(value);
            let pattern = if cond.op == CompareOp::Contains {
                format!("%{escaped}%")
            } else {
                format!("{escaped}%")
            };
            params.push(QueryValue::Text(pattern));
            Ok(format!(
                "{col_expr} LIKE ?{} COLLATE NOCASE ESCAPE '\\'",
                params.len()
            ))
        }
        CompareOp::In => {
            let ConditionValue::List(values) = &cond.value else {
                return Err(QueryError::Validation("IN requires a list literal".into()));
            };
            if values.is_empty() {
                return Ok("0".into());
            }
            let has_string = values
                .iter()
                .any(|value| matches!(value, ConditionValue::String(_)));
            let placeholders = values
                .iter()
                .map(|value| bind_condition_value(value, params).map(|index| format!("?{index}")))
                .collect::<Result<Vec<_>, _>>()?;
            let collate = if has_string { " COLLATE NOCASE" } else { "" };
            Ok(format!(
                "{col_expr}{collate} IN ({})",
                placeholders.join(", ")
            ))
        }
        CompareOp::IsNotNull => {
            if !matches!(cond.value, ConditionValue::Null) {
                return Err(QueryError::Validation(
                    "IS NOT NULL does not accept a value".into(),
                ));
            }
            Ok(format!("{col_expr} IS NOT NULL"))
        }
        op => {
            let op_str = match op {
                CompareOp::Eq => "=",
                CompareOp::Neq => "!=",
                CompareOp::Gt => ">",
                CompareOp::Lt => "<",
                CompareOp::Gte => ">=",
                CompareOp::Lte => "<=",
                CompareOp::Like => "LIKE",
                CompareOp::Contains
                | CompareOp::StartsWith
                | CompareOp::In
                | CompareOp::IsNotNull => unreachable!(),
            };
            let is_string = matches!(cond.value, ConditionValue::String(_));
            let param_index = bind_condition_value(&cond.value, params)?;
            let collate = if is_string && matches!(op, CompareOp::Eq | CompareOp::Like) {
                " COLLATE NOCASE"
            } else {
                ""
            };
            Ok(format!("{col_expr} {op_str} ?{param_index}{collate}"))
        }
    }
}

fn expr_endpoint_set(
    expr: &WhereExpr,
    start_var: Option<&str>,
    end_var: Option<&str>,
) -> (bool, bool) {
    match expr {
        WhereExpr::True => (false, false),
        WhereExpr::Condition(c) => {
            let is_start = start_var == Some(c.variable.as_str());
            let is_end = end_var == Some(c.variable.as_str());
            (is_start, is_end)
        }
        WhereExpr::And(l, r) | WhereExpr::Or(l, r) => {
            let (ls, le) = expr_endpoint_set(l, start_var, end_var);
            let (rs, re) = expr_endpoint_set(r, start_var, end_var);
            (ls || rs, le || re)
        }
    }
}

/// Return `Err(Unsupported)` if any `Or` node spans both endpoint variables.
fn reject_or_spanning_endpoints(
    expr: &WhereExpr,
    start: &NodePattern,
    end: &NodePattern,
) -> Result<(), QueryError> {
    let start_var = start.variable.as_deref();
    let end_var = end.variable.as_deref();
    reject_or_spanning_impl(expr, start_var, end_var)
}

fn reject_or_spanning_impl(
    expr: &WhereExpr,
    start_var: Option<&str>,
    end_var: Option<&str>,
) -> Result<(), QueryError> {
    match expr {
        WhereExpr::True | WhereExpr::Condition(_) => Ok(()),
        WhereExpr::And(l, r) => {
            reject_or_spanning_impl(l, start_var, end_var)?;
            reject_or_spanning_impl(r, start_var, end_var)
        }
        WhereExpr::Or(l, r) => {
            let (l_start, l_end) = expr_endpoint_set(l, start_var, end_var);
            let (r_start, r_end) = expr_endpoint_set(r, start_var, end_var);
            let spans_start = l_start || r_start;
            let spans_end = l_end || r_end;
            if spans_start && spans_end {
                return Err(QueryError::Unsupported(
                    "WHERE clauses that span both endpoints in a variable-length pattern \
                     are not yet supported; rewrite as separate queries or restrict each \
                     OR branch to one endpoint"
                        .into(),
                ));
            }
            // Even if this OR is safe, recurse to catch nested ORs.
            reject_or_spanning_impl(l, start_var, end_var)?;
            reject_or_spanning_impl(r, start_var, end_var)
        }
    }
}

fn compile_var_len_condition(
    cond: &Condition,
    start_var: Option<&str>,
    end_var: Option<&str>,
    params: &mut Vec<QueryValue>,
) -> Result<(String, &'static str), QueryError> {
    let col_alias = if start_var == Some(cond.variable.as_str()) {
        "s"
    } else if end_var == Some(cond.variable.as_str()) {
        "r"
    } else {
        return Err(QueryError::Compile(format!(
            "variable '{}' in WHERE not supported in variable-length pattern \
             (only start/end node variables)",
            cond.variable
        )));
    };

    let col_expr =
        if cond.property == "name" || cond.property == "kind" || cond.property == "entity_type" {
            format!("{col_alias}.{}", cond.property)
        } else {
            format!(
                "json_extract({col_alias}.properties, '$.{}')",
                cond.property.replace('\'', "''")
            )
        };

    let sql = compile_condition_predicate(&col_expr, cond, params)?;
    Ok((sql, col_alias))
}

/// Walk the `WhereExpr` tree for variable-length patterns, routing conditions to start or end.
fn compile_variable_length_where(
    expr: &WhereExpr,
    start_var: Option<&str>,
    end_var: Option<&str>,
    params: &mut Vec<QueryValue>,
    start_conditions: &mut Vec<String>,
    end_conditions: &mut Vec<String>,
) -> Result<Option<String>, QueryError> {
    match expr {
        WhereExpr::True => Ok(None),
        WhereExpr::Condition(cond) => {
            let (sql, alias) = compile_var_len_condition(cond, start_var, end_var, params)?;
            if alias == "s" {
                start_conditions.push(sql);
            } else {
                end_conditions.push(sql);
            }
            Ok(None)
        }
        WhereExpr::And(l, r) => {
            compile_variable_length_where(
                l,
                start_var,
                end_var,
                params,
                start_conditions,
                end_conditions,
            )?;
            compile_variable_length_where(
                r,
                start_var,
                end_var,
                params,
                start_conditions,
                end_conditions,
            )?;
            Ok(None)
        }
        WhereExpr::Or(l, r) => {
            // After reject_or_spanning_endpoints we know this Or does not straddle
            // both endpoints.  Compile each branch to a SQL string, then combine
            // with OR and push into the appropriate condition list.
            let l_sql = compile_variable_length_where_to_sql(l, start_var, end_var, params)?;
            let r_sql = compile_variable_length_where_to_sql(r, start_var, end_var, params)?;
            match (l_sql, r_sql) {
                (None, None) => {}
                (Some((ls, la)), None) => {
                    if la == "s" {
                        start_conditions.push(ls);
                    } else {
                        end_conditions.push(ls);
                    }
                }
                (None, Some((rs, ra))) => {
                    if ra == "s" {
                        start_conditions.push(rs);
                    } else {
                        end_conditions.push(rs);
                    }
                }
                (Some((ls, la)), Some((rs, _ra))) => {
                    // Both non-None and same alias (guaranteed by the spanning check).
                    let combined = format!("({ls} OR {rs})");
                    if la == "s" {
                        start_conditions.push(combined);
                    } else {
                        end_conditions.push(combined);
                    }
                }
            }
            Ok(None)
        }
    }
}

/// Compile a `WhereExpr` sub-tree to a SQL string plus the endpoint alias (`"s"` or `"r"`).
fn compile_variable_length_where_to_sql(
    expr: &WhereExpr,
    start_var: Option<&str>,
    end_var: Option<&str>,
    params: &mut Vec<QueryValue>,
) -> Result<Option<(String, &'static str)>, QueryError> {
    match expr {
        WhereExpr::True => Ok(None),
        WhereExpr::Condition(cond) => {
            let (sql, alias) = compile_var_len_condition(cond, start_var, end_var, params)?;
            Ok(Some((sql, alias)))
        }
        WhereExpr::And(l, r) => {
            let ls = compile_variable_length_where_to_sql(l, start_var, end_var, params)?;
            let rs = compile_variable_length_where_to_sql(r, start_var, end_var, params)?;
            Ok(match (ls, rs) {
                (None, None) => None,
                (Some(s), None) | (None, Some(s)) => Some(s),
                (Some((lsql, la)), Some((rsql, _))) => Some((format!("{lsql} AND {rsql}"), la)),
            })
        }
        WhereExpr::Or(l, r) => {
            let ls = compile_variable_length_where_to_sql(l, start_var, end_var, params)?;
            let rs = compile_variable_length_where_to_sql(r, start_var, end_var, params)?;
            Ok(match (ls, rs) {
                (None, None) => None,
                (Some(s), None) | (None, Some(s)) => Some(s),
                (Some((lsql, la)), Some((rsql, _))) => Some((format!("({lsql} OR {rsql})"), la)),
            })
        }
    }
}

/// Compile variable-length patterns to a recursive CTE.
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

    // Synthetic observed_as_* edges join event_observations, which has no
    // recursive path structure — reject them in variable-length patterns before
    // attempting CTE compilation (would produce a CTE over graph_edges with an
    // invalid relation string).
    if edge.relations.iter().any(|r| is_synthetic(r)) {
        return Err(QueryError::Unsupported(
            "synthetic observed_as_* edges cannot be variable-length; \
             use a fixed-length edge pattern instead"
                .into(),
        ));
    }

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
        start_conditions.push(kind_filter_predicate("s", kind, &mut params));
    }
    if let Some(ref et) = start.entity_type {
        params.push(QueryValue::Text(et.clone()));
        start_conditions.push(format!("s.entity_type = ?{}", params.len()));
    }
    let mut start_props: Vec<_> = start.properties.iter().collect();
    start_props.sort_by_key(|(k, _)| k.as_str());
    for (key, val) in start_props {
        let text_column = if key == "name" { Some("name") } else { None };
        start_conditions.push(compile_property_equality(
            "s",
            key,
            val,
            text_column,
            &mut params,
        )?);
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

    // Build the next-intermediate-node namespace filter.
    // This is applied in the recursive CTE member to prevent traversal through
    // deleted or out-of-scope intermediate nodes.  Without it, a path like
    // A -> B_deleted -> C would be returned even when B is soft-deleted.
    let next_node_ns_filter = namespace_filter("next_node", opts, &mut params);

    let max_depth_i64 = i64::try_from(max_depth)
        .map_err(|_| QueryError::InvalidInput("max_depth exceeds i64::MAX".into()))?;
    params.push(QueryValue::Integer(max_depth_i64));
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
        end_conditions.push(kind_filter_predicate("r", kind, &mut params));
    }
    if let Some(ref et) = end.entity_type {
        params.push(QueryValue::Text(et.clone()));
        end_conditions.push(format!("r.entity_type = ?{}", params.len()));
    }
    let mut end_props: Vec<_> = end.properties.iter().collect();
    end_props.sort_by_key(|(k, _)| k.as_str());
    for (key, val) in end_props {
        let text_column = if key == "name" { Some("name") } else { None };
        end_conditions.push(compile_property_equality(
            "r",
            key,
            val,
            text_column,
            &mut params,
        )?);
    }

    // WHERE clause conditions for variable-length patterns.
    // OR expressions that span both start and end nodes are not supported — reject
    // explicitly with an actionable error message rather than silently converting OR to AND.
    reject_or_spanning_endpoints(&query.where_clause, start, end)?;

    // Compile the WHERE tree preserving Or/And connectives.  After the spanning
    // check above we know every Or node touches at most one endpoint, so we can
    // safely route whole sub-trees to start_conditions or end_conditions.
    if let Some(where_sql) = compile_variable_length_where(
        &query.where_clause,
        start.variable.as_deref(),
        end.variable.as_deref(),
        &mut params,
        &mut start_conditions,
        &mut end_conditions,
    )? {
        // A non-None return means the expression spans no variable (WhereExpr::True
        // is the only such case and returns None).  This branch is unreachable given
        // the reject_or_spanning_endpoints guard above, but handle it safely.
        start_conditions.push(where_sql);
    }

    // MAJ-2: min_depth is always a bound parameter, never a literal
    if min_depth > 0 {
        let min_depth_i64 = i64::try_from(min_depth)
            .map_err(|_| QueryError::InvalidInput("min_depth exceeds i64::MAX".into()))?;
        params.push(QueryValue::Integer(min_depth_i64));
        end_conditions.push(format!("t.depth >= ?{}", params.len()));
    }

    let (limit, truncation_check) = effective_limit(query.limit, opts.max_limit);
    let limit_i64 = i64::try_from(limit)
        .map_err(|_| QueryError::InvalidInput("limit exceeds i64::MAX".into()))?;
    params.push(QueryValue::Integer(limit_i64));
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
                    if matches!(kind, VarKind::EventNode | VarKind::ObservationTargetNode) {
                        return Err(QueryError::Unsupported(
                            "synthetic observed_as_* edges cannot be used in variable-length \
                             patterns; use a fixed-length edge pattern instead"
                                .into(),
                        ));
                    }
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
                    VarKind::EventNode | VarKind::ObservationTargetNode => {
                        // Synthetic observed_as_* edges require a fixed-length pattern;
                        // variable-length recursion over the events/notes tables is not supported.
                        return Err(QueryError::Unsupported(
                            "synthetic observed_as_* edges cannot be used in variable-length \
                             patterns; use a fixed-length edge pattern instead"
                                .into(),
                        ));
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
        "JOIN primary_nodes s ON s.id = t.start_id"
    } else {
        ""
    };
    let join_end = "JOIN primary_nodes r ON r.id = t.current_id";

    // Build the next-node namespace filter clause (may be empty).
    // Already pushed into params by namespace_filter above.
    let next_node_ns_and = if next_node_ns_filter.is_empty() {
        String::new()
    } else {
        format!(" AND {}", next_node_ns_filter.trim_start_matches(" AND "))
    };

    let sql = format!(
        "WITH RECURSIVE primary_nodes AS ({primary_nodes}), \
         traverse(start_id, current_id, depth, path, total_weight, via_edge, via_relation, via_weight) AS (\
             SELECT s.id, {seed_next}, 1, s.id || ',' || {seed_next}, e.weight, \
                    e.id, e.relation, e.weight \
             FROM primary_nodes s \
             JOIN graph_edges e ON {seed_join} AND e.deleted_at IS NULL{e_ns_filter}{relation_condition} \
             WHERE {start_where} \
             UNION ALL \
             SELECT t.start_id, {recurse_next}, t.depth + 1, \
                    t.path || ',' || {recurse_next}, \
                    t.total_weight + e.weight, \
                    e.id, e.relation, e.weight \
             FROM traverse t CROSS JOIN graph_edges e \
                 ON {recurse_join} AND e.deleted_at IS NULL{e_ns_filter}{relation_condition} \
             JOIN primary_nodes next_node ON next_node.id = ({recurse_next}) \
                    AND next_node.deleted_at IS NULL{next_node_ns_and} \
             WHERE t.depth < ?{depth_param} \
               AND (',' || t.path || ',') NOT LIKE '%,' || {recurse_next} || ',%' \
         ) \
         SELECT DISTINCT {select_cols} \
         FROM traverse t \
         {join_start} {join_end} \
         WHERE {end_where} \
         ORDER BY t.depth, t.total_weight DESC, t.start_id, t.current_id \
         LIMIT ?{limit_param}",
        primary_nodes = PRIMARY_NODE_SQL,
        seed_next = seed_next,
        seed_join = seed_join,
        e_ns_filter = e_ns_filter,
        relation_condition = relation_condition,
        start_where = start_conditions.join(" AND "),
        recurse_next = recurse_next,
        recurse_join = recurse_join,
        next_node_ns_and = next_node_ns_and,
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
        truncation_check,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VarKind {
    Node,
    /// Node that maps to the `events` table (synthetic `observed_as_*` edge source).
    EventNode,
    /// Node that maps to an entity/note `event_observations` referent
    /// (synthetic `observed_as_*` edge target).
    ObservationTargetNode,
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
/// Columns available for projection on entity/note observation-target nodes
/// (synthetic edge targets, issue #468).
const OBSERVATION_TARGET_COLUMNS: &[&str] = &[
    "id",
    "namespace",
    "kind",
    "entity_type",
    "status",
    "name",
    "content",
    "salience",
    "decay_factor",
    "properties",
    "created_at",
    "updated_at",
    "referent_kind",
];
/// Columns available for projection on `events` table nodes (synthetic edge sources).
const EVENT_COLUMNS: &[&str] = &[
    "id",
    "namespace",
    "verb",
    "substrate",
    "actor",
    "kind",
    "outcome",
    "payload",
    "duration_us",
    "target_id",
    "session_id",
    "created_at",
];
const EDGE_COLUMNS: &[&str] = &["id", "source_id", "target_id", "relation", "weight"];

fn property_to_column<'a>(prop: &'a str, kind: &VarKind) -> Result<&'a str, QueryError> {
    let (valid, kind_name) = match kind {
        VarKind::Node => (NODE_COLUMNS, "node"),
        VarKind::ObservationTargetNode => (OBSERVATION_TARGET_COLUMNS, "observation target"),
        VarKind::EventNode => (EVENT_COLUMNS, "event"),
        VarKind::Edge => (EDGE_COLUMNS, "edge"),
    };
    if valid.contains(&prop) {
        Ok(prop)
    } else {
        Err(QueryError::Compile(format!(
            "unknown {kind_name} property '{prop}' in RETURN projection. \
             Valid: {}",
            valid.join(", ")
        )))
    }
}

// INLINE TEST JUSTIFICATION: Tests access private helpers (compile_fixed_length,
// compile_variable_length, compile_single_condition, compile_var_len_condition) and
// internal types (VarKind) via pub(crate) visibility; moving to crates/khive-query/tests/
// would require making those items pub, which would widen the public API surface.
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
        // Exceeding MAX_DEPTH is an InvalidInput error at validation time —
        // the compiler never sees a query with depth > 10.
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
        // Query requests 1000, max_limit is 500 — the compiler now emits a
        // sentinel LIMIT of 501 so the execution site can detect and warn on
        // real truncation (issue #777); the returned rows are still capped
        // at 500 once the sentinel is stripped at the execution site.
        let q = gql::parse("MATCH (a:concept)-[e]->(b) RETURN a LIMIT 1000").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        let limit_param = compiled.params.last().unwrap();
        assert!(
            matches!(limit_param, QueryValue::Integer(501)),
            "expected Integer(501), got {limit_param:?}"
        );
    }

    #[test]
    fn limit_over_cap_requests_sentinel_row() {
        // LIMIT 1000 against the default max_limit=500 — the cap is binding,
        // so the compiler must fetch a max_limit+1 sentinel row rather than
        // inferring truncation from the requested LIMIT alone (issue #777).
        let q = gql::parse("MATCH (a:concept)-[e]->(b) RETURN a LIMIT 1000").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert_eq!(
            compiled.truncation_check,
            Some(TruncationCheck {
                max_limit: 500,
                requested_limit: Some(1000),
            })
        );
        let limit_param = compiled.params.last().unwrap();
        assert!(
            matches!(limit_param, QueryValue::Integer(501)),
            "expected sentinel LIMIT 501, got {limit_param:?}"
        );
    }

    #[test]
    fn limit_exactly_at_cap_no_sentinel() {
        // LIMIT 500 == max_limit exactly — the cap never binds, fetch exactly
        // what was requested and no sentinel check is needed.
        let q = gql::parse("MATCH (a:concept)-[e]->(b) RETURN a LIMIT 500").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert_eq!(compiled.truncation_check, None);
        let limit_param = compiled.params.last().unwrap();
        assert!(matches!(limit_param, QueryValue::Integer(500)));
    }

    #[test]
    fn limit_below_cap_no_sentinel() {
        // Explicit LIMIT 100, well under the 500 cap — not truncated, no sentinel.
        let q = gql::parse("MATCH (a:concept)-[e]->(b) RETURN a LIMIT 100").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert_eq!(compiled.truncation_check, None);
        let limit_param = compiled.params.last().unwrap();
        assert!(matches!(limit_param, QueryValue::Integer(100)));
    }

    #[test]
    fn no_explicit_limit_requests_sentinel_row() {
        // No LIMIT clause at all — the cap is the only bound, and the true
        // match count is unknown at compile time, so the compiler must fetch
        // a sentinel row to let the execution site detect real truncation
        // (this is issue #777's original silent-truncation case).
        let q = gql::parse("MATCH (a:concept)-[e]->(b) RETURN a").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert_eq!(
            compiled.truncation_check,
            Some(TruncationCheck {
                max_limit: 500,
                requested_limit: None,
            })
        );
        let limit_param = compiled.params.last().unwrap();
        assert!(
            matches!(limit_param, QueryValue::Integer(501)),
            "expected sentinel LIMIT 501, got {limit_param:?}"
        );
    }

    #[test]
    fn variable_length_limit_over_cap_requests_sentinel_row() {
        // Same sentinel-row coverage for the variable-length/traversal compile path.
        let q = gql::parse("MATCH (a)-[:extends*1..3]->(b) RETURN b LIMIT 5000").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert_eq!(
            compiled.truncation_check,
            Some(TruncationCheck {
                max_limit: 500,
                requested_limit: Some(5000),
            })
        );
        let limit_param = compiled.params.last().unwrap();
        assert!(
            matches!(limit_param, QueryValue::Integer(501)),
            "expected sentinel LIMIT 501, got {limit_param:?}"
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
        // references `r.deleted_at` / `r.namespace`, so primary_nodes r must be
        // joined unconditionally.
        let q = gql::parse("MATCH (a:concept)-[:extends*1..3]->(b) RETURN a LIMIT 10").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.contains("JOIN primary_nodes r"),
            "primary_nodes r must always be joined when r.* conditions are emitted; sql: {}",
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

    /// Regression guard for ISSUE #231: SPARQL subject→predicate→object direction.
    /// `?a :extends ?b` must bind ?a to source_id and ?b to target_id, not swapped.
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
            compiled.sql.contains("ON n1.id = e0.target_id"),
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

    // --- OR support in WHERE clause ---

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

    // --- event_observations synthetic edge support ---

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

    // CRIT-1 regression: event source node must bind to `events` table, not `entities`.
    // Previously `FROM entities n0 JOIN event_observations e0 ON e0.event_id = n0.id`
    // was emitted — IDs are disjoint so every query returned zero rows.
    #[test]
    fn synthetic_edge_event_source_binds_events_table() {
        let q = gql::parse("MATCH (ev)-[:observed_as_selected]->(m:memory) RETURN ev, m").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.contains("FROM events "),
            "CRIT-1: event source must come FROM events table, not entities; sql: {}",
            compiled.sql
        );
        assert!(
            !compiled
                .sql
                .starts_with("SELECT * FROM entities n0 JOIN event_observations"),
            "CRIT-1: must not join events via entities table; sql: {}",
            compiled.sql
        );
    }

    #[test]
    fn synthetic_edge_event_observation_join_uses_events_id() {
        // The JOIN must be `event_observations.event_id = events_alias.id`,
        // not `event_observations.event_id = entities_alias.id`.
        let q = gql::parse("MATCH (ev)-[:observed_as_selected]->(m) RETURN m").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        // The event alias is n0; the join must reference n0 against `events` table.
        assert!(
            compiled
                .sql
                .contains("JOIN event_observations e0 ON e0.event_id = n0.id"),
            "CRIT-1: event_observations must join on events.id (n0 is now events); sql: {}",
            compiled.sql
        );
    }

    #[test]
    fn synthetic_edge_event_node_projects_event_columns() {
        // The event variable in RETURN must select event-table columns (verb, outcome, …),
        // not entity columns (name, entity_type, properties, …).
        let q = gql::parse("MATCH (ev)-[:observed_as_selected]->(m) RETURN ev").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.contains("ev_verb"),
            "CRIT-1: event variable must project verb column; sql: {}",
            compiled.sql
        );
        assert!(
            compiled.sql.contains("ev_outcome"),
            "CRIT-1: event variable must project outcome column; sql: {}",
            compiled.sql
        );
        assert!(
            !compiled.sql.contains("ev_name,") && !compiled.sql.contains("ev_name "),
            "CRIT-1: event variable must NOT project entity name column; sql: {}",
            compiled.sql
        );
        assert!(
            !compiled.sql.contains("ev_properties"),
            "CRIT-1: event variable must NOT project entity properties column; sql: {}",
            compiled.sql
        );
    }

    #[test]
    fn synthetic_edge_namespace_filter_on_events_table() {
        // MIN-2: when scoped, the namespace filter must target the events table
        // (which has a namespace column) — not rely on entities indirection.
        let q = gql::parse("MATCH (ev)-[:observed_as_selected]->(m) RETURN m").unwrap();
        let compiled = compile(&q, &scoped("test-ns")).unwrap();
        // Both the event alias (n0, now from `events`) and the target alias (n1, from `entities`)
        // must have namespace filters.
        let ns_count = compiled
            .params
            .iter()
            .filter(|p| matches!(p, QueryValue::Text(s) if s == "test-ns"))
            .count();
        assert!(
            ns_count >= 2,
            "MIN-2: namespace must be filtered on both events and target; params: {:?}",
            compiled.params
        );
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

    // --- MAJ-1: OR spanning both endpoints in variable-length patterns must be rejected ---

    #[test]
    fn variable_length_or_across_endpoints_rejected() {
        // MAJ-1: `WHERE a.name='X' OR b.name='Y'` in a variable-length pattern must be
        // rejected with Unsupported — not silently compiled to AND.
        let q = gql::parse(
            "MATCH (a)-[:extends*1..3]->(b) WHERE a.name = 'X' OR b.name = 'Y' RETURN a",
        )
        .unwrap();
        let result = compile(&q, &opts());
        assert!(
            matches!(result, Err(QueryError::Unsupported(_))),
            "MAJ-1: OR spanning both endpoints must return Unsupported; got {result:?}"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("separate queries") || err_msg.contains("one endpoint"),
            "error must be actionable; got: {err_msg}"
        );
    }

    #[test]
    fn variable_length_or_single_endpoint_still_works() {
        // OR within a single endpoint (same alias) must still compile successfully.
        let q = gql::parse(
            "MATCH (a)-[:extends*1..3]->(b) WHERE a.name = 'X' OR a.name = 'Y' RETURN a",
        )
        .unwrap();
        let result = compile(&q, &opts());
        assert!(
            result.is_ok(),
            "single-endpoint OR must compile; got {result:?}"
        );
    }

    #[test]
    fn variable_length_and_across_endpoints_still_works() {
        // AND across endpoints must still compile (the existing behavior is correct for AND).
        let q = gql::parse(
            "MATCH (a)-[:extends*1..3]->(b) WHERE a.name = 'X' AND b.name = 'Y' RETURN a",
        )
        .unwrap();
        let result = compile(&q, &opts());
        assert!(
            result.is_ok(),
            "AND across endpoints must compile; got {result:?}"
        );
    }

    // --- Regression tests for #379: variable-length WHERE OR must not flatten to AND ---

    #[test]
    fn test_variable_length_or_compiles_to_or() {
        // #379: MATCH (a)-[*1..3 WHERE p1 OR p2]-> in GQL surface maps to a single-endpoint
        // OR in the WHERE clause.  The compiled SQL must contain OR, not AND.
        let q = gql::parse(
            "MATCH (a)-[:extends*1..3]->(b) WHERE a.name = 'LoRA' OR a.name = 'QLoRA' RETURN b",
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        // The start_conditions list must contain an OR fragment, not two AND-joined conditions.
        assert!(
            compiled.sql.contains(" OR "),
            "#379: variable-length single-endpoint OR must produce SQL OR; sql: {}",
            compiled.sql
        );
        // Both values must appear as bound parameters.
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
    fn test_single_endpoint_or_at_depth_1() {
        // #379: single-hop pattern with single-endpoint OR in WHERE.
        // The OR must appear in the compiled SQL (not silently become AND).
        let q = gql::parse(
            "MATCH (a)-[r:extends]->(b) WHERE r.weight > 0.5 OR r.relation = 'extends' RETURN a",
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.contains(" OR "),
            "#379: fixed-length single-endpoint OR must produce SQL OR; sql: {}",
            compiled.sql
        );
        let has_extends = compiled
            .params
            .iter()
            .any(|p| matches!(p, QueryValue::Text(s) if s == "extends"));
        assert!(
            has_extends,
            "relation value 'extends' must be a bound param"
        );
    }

    #[test]
    fn test_and_still_works() {
        // #379: regression guard — simple WHERE p1 AND p2 must still emit AND.
        let q = gql::parse(
            "MATCH (a)-[:extends*1..3]->(b) WHERE a.name = 'LoRA' AND a.kind = 'concept' RETURN b",
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        // The SQL must not contain a bare " OR " from the AND expression.
        assert!(
            !compiled.sql.contains(" OR "),
            "#379: AND must not produce OR; sql: {}",
            compiled.sql
        );
        let has_lora = compiled
            .params
            .iter()
            .any(|p| matches!(p, QueryValue::Text(s) if s == "LoRA"));
        let has_concept = compiled
            .params
            .iter()
            .any(|p| matches!(p, QueryValue::Text(s) if s == "concept"));
        assert!(
            has_lora && has_concept,
            "both AND values must be bound params"
        );
    }

    // --- Regression tests for P0/P1 correctness fixes ---

    /// max_limit overflow: usize::MAX as i64 == -1 on 64-bit, defeating the cap.
    #[test]
    fn max_limit_overflow_returns_error() {
        let q = gql::parse("MATCH (a)-[:extends]->(b) RETURN a").unwrap();
        let opts = CompileOptions {
            scopes: vec![],
            max_limit: usize::MAX,
        };
        // On 64-bit: usize::MAX > i64::MAX, so try_from must return Err.
        // On 32-bit: usize::MAX == u32::MAX which fits in i64, so this may succeed —
        // either way we must not produce a negative limit.
        let result = compile(&q, &opts);
        match result {
            Err(QueryError::InvalidInput(_)) => {
                // Expected on 64-bit: overflow detected, error returned.
            }
            Ok(compiled) => {
                // On 32-bit: limit fits in i64 — verify it is non-negative.
                let limit_param = compiled.params.last().unwrap();
                assert!(
                    matches!(limit_param, QueryValue::Integer(n) if *n >= 0),
                    "limit must never be negative; got {limit_param:?}"
                );
            }
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }

    /// max_limit=0 with no query limit: the cap is binding (0 rows allowed),
    /// so the compiler fetches a single sentinel row (max_limit+1 = 1) to
    /// detect whether any row exists at all, no crash.
    #[test]
    fn max_limit_zero_compiles() {
        let q = gql::parse("MATCH (a)-[:extends]->(b) RETURN a").unwrap();
        let opts = CompileOptions {
            scopes: vec![],
            max_limit: 0,
        };
        let compiled = compile(&q, &opts).unwrap();
        assert_eq!(
            compiled.truncation_check,
            Some(TruncationCheck {
                max_limit: 0,
                requested_limit: None,
            })
        );
        let limit_param = compiled.params.last().unwrap();
        assert!(
            matches!(limit_param, QueryValue::Integer(1)),
            "max_limit=0 should produce sentinel LIMIT 1; got {limit_param:?}"
        );
    }

    /// Variable-length synthetic edges must be rejected.
    #[test]
    fn variable_length_synthetic_edge_rejected() {
        // observed_as_selected*1..3 must be rejected — the recursive CTE targets
        // graph_edges, which has no event_observations data.
        let q = gql::parse("MATCH (ev)-[:observed_as_selected*1..3]->(m) RETURN m").unwrap();
        let err = compile(&q, &opts()).unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "variable-length synthetic edge must return Unsupported; got {err:?}"
        );
        assert!(
            err.to_string().contains("synthetic") || err.to_string().contains("observed_as"),
            "error should mention synthetic edges: {err}"
        );
    }

    /// Variable-length traversal must not pass through deleted intermediate nodes.
    /// The compiled SQL must join entities for the next node in the recursive member.
    #[test]
    fn variable_length_recursive_member_joins_next_node_for_deleted_filter() {
        let q = gql::parse("MATCH (a)-[:extends*1..3]->(b) RETURN b").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        // The recursive CTE member must join next_node to filter deleted intermediates.
        assert!(
            compiled.sql.contains("JOIN primary_nodes next_node"),
            "recursive CTE must join primary_nodes next_node for deleted-intermediate filtering; sql: {}",
            compiled.sql
        );
        assert!(
            compiled.sql.contains("next_node.deleted_at IS NULL"),
            "recursive CTE must filter next_node.deleted_at IS NULL; sql: {}",
            compiled.sql
        );
    }

    /// Variable-length traversal with namespace scope: the next_node join must
    /// also apply the namespace filter to prevent namespace-crossing intermediates.
    #[test]
    fn variable_length_recursive_member_namespace_scopes_intermediates() {
        let q = gql::parse("MATCH (a)-[:extends*1..3]->(b) RETURN b").unwrap();
        let compiled = compile(&q, &scoped("test-ns")).unwrap();
        // The next_node join must include a namespace condition.
        assert!(
            compiled.sql.contains("next_node.namespace"),
            "recursive CTE next_node join must filter namespace; sql: {}",
            compiled.sql
        );
    }

    /// Public AST panic: compile must return an error for a malformed AST instead
    /// of panicking with an out-of-bounds index.
    #[test]
    fn compile_malformed_ast_returns_error_not_panic() {
        use crate::ast::{EdgeDirection, EdgePattern, GqlQuery, MatchPattern, PatternElement};
        // An AST that starts with an Edge (no leading Node) is malformed.
        let q = GqlQuery {
            pattern: MatchPattern {
                elements: vec![PatternElement::Edge(EdgePattern {
                    variable: None,
                    relations: vec!["extends".to_string()],
                    direction: EdgeDirection::Out,
                    min_hops: 1,
                    max_hops: 1,
                })],
            },
            where_clause: WhereExpr::True,
            return_items: vec![],
            limit: None,
        };
        let result = compile(&q, &opts());
        assert!(
            result.is_err(),
            "malformed AST (starts with Edge) must return error, not panic"
        );
    }

    /// GQL edge pattern suffix fix: `(a)-[e:extends](b)` must be rejected because
    /// the `-` suffix after `]` is required.
    #[test]
    fn edge_pattern_without_suffix_dash_rejected() {
        let result = gql::parse("MATCH (a)-[e:extends](b) RETURN a");
        assert!(
            result.is_err(),
            "edge pattern without suffix '-' must be rejected as a parse error"
        );
    }

    // --- Read-only invariant regression tests (#16) ---

    /// assert_select_only accepts SELECT and WITH (recursive CTE).
    #[test]
    fn assert_select_only_accepts_select_and_with() {
        assert!(
            assert_select_only("SELECT a FROM entities WHERE 1=1").is_ok(),
            "SELECT must be accepted"
        );
        assert!(
            assert_select_only("WITH RECURSIVE traverse AS (...) SELECT ...").is_ok(),
            "WITH must be accepted (recursive CTE)"
        );
    }

    /// assert_select_only rejects write SQL with the canonical read-only message.
    #[test]
    fn assert_select_only_rejects_write_sql_with_readonly_message() {
        for stmt in &[
            "INSERT INTO entities VALUES (?)",
            "UPDATE entities SET name = ?",
            "DELETE FROM entities WHERE id = ?",
            "DROP TABLE entities",
        ] {
            let err = assert_select_only(stmt).unwrap_err();
            assert!(
                matches!(err, QueryError::Compile(_)),
                "write SQL must return Compile error for '{stmt}'; got {err:?}"
            );
            let msg = err.to_string();
            assert!(
                msg.contains("read-only"),
                "error must mention 'read-only' for '{stmt}'; got: {msg}"
            );
            assert!(
                msg.contains("create") && msg.contains("delete"),
                "error must name the mutation verbs for '{stmt}'; got: {msg}"
            );
        }
    }

    /// Regression: compile() for a valid GQL query must still succeed end-to-end.
    /// This guards against assert_select_only incorrectly rejecting compiler output.
    #[test]
    fn readonly_guard_does_not_break_valid_gql_compile() {
        let q = gql::parse("MATCH (a:concept)-[:extends]->(b) RETURN a LIMIT 10").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.starts_with("SELECT"),
            "valid GQL must compile to SELECT; sql: {}",
            compiled.sql
        );
    }

    /// Regression: compile() for a variable-length GQL query must still succeed.
    #[test]
    fn readonly_guard_does_not_break_valid_cte_compile() {
        let q = gql::parse("MATCH (a)-[:extends*1..3]->(b) RETURN b LIMIT 10").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.starts_with("WITH RECURSIVE"),
            "variable-length GQL must compile to WITH RECURSIVE; sql: {}",
            compiled.sql
        );
    }

    /// GQL write forms are rejected at the parse layer before reaching the compiler.
    #[test]
    fn gql_write_form_rejected_before_compile() {
        use crate::parsers::gql;
        let err = gql::parse("CREATE (n:concept) RETURN n").unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "GQL CREATE must be Unsupported; got {err:?}"
        );
        assert!(
            err.to_string().contains("read-only"),
            "error must mention read-only; got: {err}"
        );
    }

    /// SPARQL write forms are rejected at the parse layer before reaching the compiler.
    #[test]
    fn sparql_write_form_rejected_before_compile() {
        use crate::parsers::sparql;
        let err = sparql::parse("INSERT DATA { ?a :extends ?b }").unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "SPARQL INSERT must be Unsupported; got {err:?}"
        );
        assert!(
            err.to_string().contains("read-only"),
            "error must mention read-only; got: {err}"
        );
    }

    /// Duplicate inline property rejection.
    #[test]
    fn duplicate_inline_property_rejected() {
        let result = gql::parse("MATCH (n {name: 'A', name: 'B'}) RETURN n");
        assert!(
            result.is_err(),
            "duplicate property 'name' in node props must be rejected"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("duplicate") || err.contains("name"),
            "error should mention duplicate or key name: {err}"
        );
    }

    /// Unknown synthetic relation must be rejected at validation.
    #[test]
    fn unknown_synthetic_relation_rejected_at_compile() {
        let q = gql::parse("MATCH (a)-[:observed_as_bogus]->(b) RETURN a").unwrap();
        let err = compile(&q, &opts()).unwrap_err();
        assert!(
            matches!(err, QueryError::Validation(_)),
            "unknown synthetic relation must return Validation error; got {err:?}"
        );
    }

    // --- Issue #467: canonical edge queries must admit legal note endpoints ---

    /// `annotates` is the cross-substrate relation: note -> entity/note/event/edge.
    /// Endpoints must not be hard-bound to `entities` only.
    #[test]
    fn canonical_edge_annotates_compiles_note_source_and_any_target_substrate() {
        let q = gql::parse("MATCH (n)-[:annotates]->(x) RETURN n.id, x.id LIMIT 10").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            !compiled.sql.contains("FROM entities n0"),
            "endpoint must not be hard-bound to entities only; sql: {}",
            compiled.sql
        );
        for table in ["FROM notes", "FROM events", "FROM graph_edges"] {
            assert!(
                compiled.sql.contains(table),
                "endpoint source must admit {table} rows for annotates; sql: {}",
                compiled.sql
            );
        }
    }

    /// `supports` is same-substrate: note -> note (or entity -> entity). A
    /// legal note-note pair must be able to bind through the endpoint source.
    #[test]
    fn canonical_edge_supports_compiles_note_note_endpoints() {
        let q = gql::parse("MATCH (a)-[:supports]->(b) RETURN a.id, b.id LIMIT 10").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert_eq!(
            compiled.sql.matches("FROM notes").count(),
            2,
            "both supports endpoints must admit note rows; sql: {}",
            compiled.sql
        );
    }

    /// Pack-declared `depends_on` on task-kind notes must bind through the
    /// endpoint source rather than being restricted to `entities`.
    #[test]
    fn canonical_edge_depends_on_compiles_pack_task_note_endpoints() {
        let q = gql::parse("MATCH (a:task)-[:depends_on]->(b:task) RETURN a.id, b.id LIMIT 10")
            .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert_eq!(
            compiled.sql.matches("FROM notes").count(),
            2,
            "task-kind depends_on endpoints must admit note rows; sql: {}",
            compiled.sql
        );
        let task_params = compiled
            .params
            .iter()
            .filter(|p| matches!(p, QueryValue::Text(s) if s == "task"))
            .count();
        assert_eq!(
            task_params, 2,
            "kind='task' must be bound for both endpoints"
        );
    }

    /// Variable-length canonical traversal must bind seed, intermediate, and
    /// final endpoints to the primary substrate union, not `entities` only.
    #[test]
    fn variable_length_canonical_compiles_primary_substrate_nodes() {
        let q = gql::parse("MATCH (a)-[:supports*1..2]->(b) RETURN b.id LIMIT 10").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            !compiled.sql.contains("FROM entities s"),
            "seed must not be hard-bound to entities only; sql: {}",
            compiled.sql
        );
        assert!(
            compiled.sql.contains("JOIN primary_nodes next_node"),
            "intermediate must join primary_nodes; sql: {}",
            compiled.sql
        );
        assert!(
            compiled.sql.contains("JOIN primary_nodes r"),
            "final endpoint must join primary_nodes; sql: {}",
            compiled.sql
        );
    }

    // --- Issue #468: observed_as_target/observed_as_signal must admit entity referents ---

    /// `observed_as_target` admits entity OR note referents (event.substrate
    /// determines the legal referent_kind); the target must not be forced to notes.
    #[test]
    fn synthetic_edge_observed_as_target_compiles_entity_referents() {
        let q = gql::parse("MATCH (ev)-[:observed_as_target]->(t) RETURN t.id LIMIT 10").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            !compiled
                .sql
                .contains("JOIN notes n1 ON n1.id = e0.entity_id AND e0.referent_kind = 'note'"),
            "target join must not be hard-bound to notes; sql: {}",
            compiled.sql
        );
        assert!(
            compiled.sql.contains("FROM entities"),
            "observation target source must admit entity referents; sql: {}",
            compiled.sql
        );
        assert!(
            compiled.sql.contains("n1.referent_kind = e0.referent_kind"),
            "target join must discriminate by referent_kind instead of hard-coding 'note'; sql: {}",
            compiled.sql
        );
    }

    /// `observed_as_signal` admits entity OR note referents (ADR-041: brain
    /// feedback signals may target either substrate).
    #[test]
    fn synthetic_edge_observed_as_signal_compiles_entity_and_note_referents() {
        let q = gql::parse("MATCH (ev)-[:observed_as_signal]->(t) RETURN t.id LIMIT 10").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled
                .sql
                .contains("e0.role = 'signal' AND e0.referent_kind IN ('entity', 'note')"),
            "signal role must be admitted against entity or note referents; sql: {}",
            compiled.sql
        );
    }

    /// `observed_as_selected` must still be restricted to note referents —
    /// the fix must not widen recall/rerank observations to entities.
    #[test]
    fn synthetic_edge_observed_as_selected_still_compiles_note_referents() {
        let q = gql::parse("MATCH (ev)-[:observed_as_selected]->(m:memory) RETURN m.id LIMIT 10")
            .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled
                .sql
                .contains("e0.role IN ('candidate', 'selected') AND e0.referent_kind = 'note'"),
            "selected/candidate roles must still be guarded to note referents only; sql: {}",
            compiled.sql
        );
    }
}
