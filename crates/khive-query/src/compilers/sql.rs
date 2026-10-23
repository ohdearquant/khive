//! Parameterized SQL compilation for fixed and variable-length query patterns.

use crate::ast::*;
use crate::error::QueryError;
use crate::language::read_only_error;
use crate::validate::{validate_with_warnings, MAX_DEPTH, SYNTHETIC_RELATIONS};

#[path = "mailbox.rs"]
mod mailbox;
use mailbox::MailboxLowering;

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

/// Substrate-agnostic source for canonical edge endpoints (issue #467).
fn primary_node_sql(mailbox: Option<&MailboxLowering>) -> String {
    let note_filter = mailbox.map_or_else(String::new, |scope| scope.note_filter("notes"));
    let edge_filter = mailbox.map_or_else(String::new, |scope| scope.edge_filter("graph_edges"));
    format!(
        "\
    SELECT id, namespace, kind, entity_type, name, description, NULL AS content, \
           NULL AS status, NULL AS salience, NULL AS decay_factor, properties, \
           created_at, updated_at, deleted_at, 'entity' AS substrate_kind \
      FROM entities \
    UNION ALL \
    SELECT id, namespace, kind, NULL AS entity_type, name, NULL AS description, \
           content, status, salience, decay_factor, properties, \
           created_at, updated_at, deleted_at, 'note' AS substrate_kind \
      FROM notes{note_filter} \
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
      FROM graph_edges{edge_filter}"
    )
}

fn primary_node_source(alias: &str, mailbox: Option<&MailboxLowering>) -> String {
    format!("({}) {alias}", primary_node_sql(mailbox))
}

/// Entity/note referent source for synthetic observation targets (issue #468).
fn observation_target_sql(mailbox: Option<&MailboxLowering>) -> String {
    let note_filter = mailbox.map_or_else(String::new, |scope| scope.note_filter("notes"));
    format!(
        "\
    SELECT id, namespace, kind, entity_type, name, description, \
           NULL AS content, NULL AS status, NULL AS salience, \
           NULL AS decay_factor, properties, created_at, updated_at, \
           deleted_at, 'entity' AS referent_kind \
      FROM entities \
    UNION ALL \
    SELECT id, namespace, kind, NULL AS entity_type, name, NULL AS description, \
           content, status, salience, decay_factor, properties, \
           created_at, updated_at, deleted_at, 'note' AS referent_kind \
      FROM notes{note_filter}"
    )
}

fn observation_target_source(alias: &str, mailbox: Option<&MailboxLowering>) -> String {
    format!("({}) {alias}", observation_target_sql(mailbox))
}

fn edge_source(alias: &str, mailbox: Option<&MailboxLowering>) -> String {
    match mailbox {
        Some(scope) => format!(
            "(SELECT * FROM graph_edges{}) {alias}",
            scope.edge_filter("graph_edges")
        ),
        None => format!("graph_edges {alias}"),
    }
}

/// Quotes one SQL identifier while preserving the returned column name.
fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// Parameterized read-only SQL plus the metadata required to decode its result.
///
/// See `crates/khive-query/docs/api/sql-compilation.md` for lowering rules.
#[derive(Debug)]
pub struct CompiledQuery {
    /// SQL statement using positional `?N` placeholders.
    pub sql: String,
    /// Parameters in placeholder order.
    pub params: Vec<QueryValue>,
    /// Caller-requested projections in result-column order.
    pub return_vars: Vec<ReturnItem>,
    /// Non-fatal validation or compilation diagnostics.
    pub warnings: Vec<String>,
    /// Sentinel metadata when SQL fetches `max_limit + 1` to detect another page.
    pub truncation_check: Option<TruncationCheck>,
}

/// Instructions for stripping a row-cap sentinel after execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TruncationCheck {
    /// Effective page size to enforce after detecting and removing the sentinel.
    pub max_limit: usize,
    /// Explicit caller limit, retained for diagnostics.
    pub requested_limit: Option<usize>,
}

/// Caller-derived mailbox partition applied by [`compile_with_mailbox`].
///
/// The compiler does not authenticate this scope. The runtime must derive it
/// from its authorized caller, never from query text or query arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryMailboxScope {
    /// Exact caller actor label; SQL always binds it as a text parameter.
    pub actor_id: String,
    /// Whether the anonymous, non-delegated local caller may see legacy rows.
    pub legacy_local: bool,
    /// Trusted configured maintainer copied from the authorized mailbox view.
    pub maintainer_actor: Option<String>,
}

/// Runtime options injected by the caller to scope and cap query execution.
pub struct CompileOptions {
    /// Namespace scope; empty means all namespaces.
    pub scopes: Vec<String>,
    /// Effective server page size applied against any explicit query limit.
    ///
    /// The public handler clamps its requested `page_size` to the hard cap before
    /// constructing these compiler options.
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

/// Chooses the SQL limit and optional cap-sentinel check (issue #777).
///
/// An explicit query-text `LIMIT` is a TOTAL bound across every `SKIP` page, not a
/// page-local one (ADR-008 §"query LIMIT and page_size composition", docs/guide/api-reference.md).
/// `offset` is the page's `SKIP` value; the SQL bound is `min(limit - offset, max_limit)`,
/// and the cap sentinel (`max_limit + 1`) is requested only when another page could still
/// exist within the query's own limit. An offset at or beyond the limit yields SQL `LIMIT 0`
/// — a terminal empty page, never an error.
/// See `crates/khive-query/docs/api/sql-compilation.md` for lifecycle details.
fn effective_limit(
    requested_limit: Option<usize>,
    offset: usize,
    max_limit: usize,
) -> (usize, Option<TruncationCheck>) {
    match requested_limit {
        Some(limit) => {
            let remaining = limit.saturating_sub(offset);
            if remaining <= max_limit {
                (remaining, None)
            } else {
                (
                    max_limit.saturating_add(1),
                    Some(TruncationCheck {
                        max_limit,
                        requested_limit: Some(limit),
                    }),
                )
            }
        }
        None => (
            max_limit.saturating_add(1),
            Some(TruncationCheck {
                max_limit,
                requested_limit: None,
            }),
        ),
    }
}

/// Compiles `query` to parameterized, read-only SQL under `opts`.
///
/// The returned parameter vector is ordered to match the statement's `?N` placeholders.
///
/// # Errors
///
/// Returns [`QueryError`] when validation fails, a construct is unsupported,
/// a projection cannot be lowered, or a bound limit/value is invalid.
/// See `crates/khive-query/docs/api/sql-compilation.md` for compilation paths.
pub fn compile(query: &GqlQuery, opts: &CompileOptions) -> Result<CompiledQuery, QueryError> {
    compile_scoped(query, opts, None)
}

/// Compiles under a caller-derived message partition before matching and paging.
///
/// This is a pure lowering API, not an authorization or actor-resolution boundary.
/// Namespace filtering and all other [`compile`] behavior remain unchanged.
///
/// # Errors
///
/// Returns the same validation and lowering errors as [`compile`].
pub fn compile_with_mailbox(
    query: &GqlQuery,
    opts: &CompileOptions,
    mailbox: &QueryMailboxScope,
) -> Result<CompiledQuery, QueryError> {
    compile_scoped(query, opts, Some(mailbox))
}

fn compile_scoped(
    query: &GqlQuery,
    opts: &CompileOptions,
    mailbox: Option<&QueryMailboxScope>,
) -> Result<CompiledQuery, QueryError> {
    if query.pattern.elements.is_empty() {
        return Err(QueryError::Compile("empty pattern".into()));
    }

    let mut query = query.clone();
    let warnings = validate_with_warnings(&mut query)?;

    let mut compiled = if query.pattern.has_variable_length() {
        compile_variable_length(&query, opts, mailbox)?
    } else {
        compile_fixed_length(&query, opts, mailbox)?
    };
    compiled.warnings.extend(warnings);

    // Fail closed if a future lowering path accidentally emits a mutation.
    assert_select_only(&compiled.sql)?;

    Ok(compiled)
}

/// Enforces the compiler's SELECT/WITH-only invariant; the reader is the security boundary.
fn assert_select_only(sql: &str) -> Result<(), QueryError> {
    let first = sql.split_whitespace().next().unwrap_or("").to_uppercase();
    if first == "SELECT" || first == "WITH" {
        return Ok(());
    }
    Err(read_only_error(QueryError::Compile))
}

fn in_placeholders(indices: impl IntoIterator<Item = usize>) -> String {
    indices
        .into_iter()
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn namespace_filter(alias: &str, opts: &CompileOptions, params: &mut Vec<QueryValue>) -> String {
    if opts.scopes.is_empty() {
        String::new()
    } else if opts.scopes.len() == 1 {
        params.push(QueryValue::Text(opts.scopes[0].clone()));
        format!(" AND {alias}.namespace = ?{}", params.len())
    } else {
        let placeholders = in_placeholders(opts.scopes.iter().map(|s| {
            params.push(QueryValue::Text(s.clone()));
            params.len()
        }));
        format!(" AND {alias}.namespace IN ({})", placeholders)
    }
}

/// Maps substrate labels to the union discriminator and granular labels to `kind`.
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

/// Applies kind filtering to the entity/note observation-target union.
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

/// Compiles typed inline-map equality against a direct or JSON property column.
/// See `crates/khive-query/docs/api/sql-compilation.md` for storage-class rules.
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

/// Compiles fixed-length patterns to a JOIN chain.
fn compile_fixed_length(
    query: &GqlQuery,
    opts: &CompileOptions,
    mailbox: Option<&QueryMailboxScope>,
) -> Result<CompiledQuery, QueryError> {
    let mut params: Vec<QueryValue> = Vec::new();
    let mailbox = mailbox.map(|scope| MailboxLowering::bind(scope, &mut params));
    let mailbox = mailbox.as_ref();
    let mut from_parts: Vec<String> = Vec::new();
    let mut join_parts: Vec<String> = Vec::new();
    let mut where_parts: Vec<String> = Vec::new();
    let mut select_parts: Vec<String> = Vec::new();
    let mut order_parts: Vec<String> = Vec::new();

    let mut node_aliases: Vec<String> = Vec::new();
    let mut var_to_alias: std::collections::HashMap<String, (String, VarKind)> =
        std::collections::HashMap::new();

    // Synthetic endpoints bind different substrate sources (issue #468).
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

                // Paging requires a total order over match identities, not caller
                // projections. UUIDs can overlap across unioned substrates, so keep
                // each union's discriminator ahead of its ID.
                if is_event_source {
                    order_parts.push(format!("{alias}.id"));
                } else if is_observation_target {
                    order_parts.push(format!("{alias}.referent_kind"));
                    order_parts.push(format!("{alias}.id"));
                } else {
                    order_parts.push(format!("{alias}.substrate_kind"));
                    order_parts.push(format!("{alias}.id"));
                }

                if node_idx == 0 {
                    if is_event_source {
                        from_parts.push(format!("events {alias}"));
                    } else if !is_observation_target {
                        from_parts.push(primary_node_source(&alias, mailbox));
                    }
                }

                if is_event_source {
                    // Events have no `deleted_at`; namespace is filtered directly.
                    let ns_filter = namespace_filter(&alias, opts, &mut params);
                    if !ns_filter.is_empty() {
                        where_parts.push(ns_filter.trim_start_matches(" AND ").to_string());
                    }
                    // Bare `event` needs no kind predicate on an event-only source.
                    if let Some(ref kind) = np.kind {
                        if kind != "event" {
                            params.push(QueryValue::Text(kind.clone()));
                            where_parts.push(format!("{alias}.kind = ?{}", params.len()));
                        }
                    }
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

                // The synthetic and canonical backing tables have no meaningful shared join key.
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
                    // `(event_id, role, position)` is the observation primary key.
                    order_parts.push(format!("{e_alias}.event_id"));
                    order_parts.push(format!("{e_alias}.role"));
                    order_parts.push(format!("{e_alias}.position"));
                    // Synthetic projections are always event-to-referent.
                    if !matches!(ep.direction, EdgeDirection::Out) {
                        return Err(QueryError::Compile(
                            "synthetic observed_as_* edges are always event → entity (outbound only)".into(),
                        ));
                    }
                    join_parts.push(format!(
                        "JOIN event_observations {e_alias} ON {e_alias}.event_id = {prev_node}.id"
                    ));
                    let roles: Vec<&'static str> = ep
                        .relations
                        .iter()
                        .filter_map(|r| synthetic_role(r))
                        .collect();
                    if roles.len() == 1 {
                        params.push(QueryValue::Text(roles[0].to_string()));
                        where_parts.push(format!("{e_alias}.role = ?{}", params.len()));
                    } else if roles.len() > 1 {
                        let placeholders = in_placeholders(roles.iter().map(|r| {
                            params.push(QueryValue::Text(r.to_string()));
                            params.len()
                        }));
                        where_parts.push(format!("{e_alias}.role IN ({})", placeholders));
                    }
                    // `referent_kind` prevents equal IDs on different substrates from colliding.
                    join_parts.push(format!(
                        "JOIN {} ON {next_alias}.id = {e_alias}.entity_id \
                         AND {next_alias}.referent_kind = {e_alias}.referent_kind",
                        observation_target_source(&next_alias, mailbox)
                    ));
                    // Enforce the ADR-041 role/substrate matrix.
                    where_parts.push(format!(
                        "(({e_alias}.role IN ('candidate', 'selected') AND {e_alias}.referent_kind IN ('entity', 'note')) \
                          OR ({e_alias}.role = 'target' AND {e_alias}.referent_kind IN ('entity', 'note')) \
                          OR ({e_alias}.role = 'signal' AND {e_alias}.referent_kind IN ('entity', 'note')))"
                    ));
                } else {
                    order_parts.push(format!("{e_alias}.id"));
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
                        "JOIN {} ON {source_join} AND {e_alias}.deleted_at IS NULL",
                        edge_source(&e_alias, mailbox)
                    ));

                    let ens_filter = namespace_filter(&e_alias, opts, &mut params);
                    if !ens_filter.is_empty() {
                        where_parts.push(ens_filter.trim_start_matches(" AND ").to_string());
                    }

                    join_parts.push(format!(
                        "JOIN {} ON {next_alias}.id = {next_join_col}",
                        primary_node_source(&next_alias, mailbox)
                    ));

                    if !ep.relations.is_empty() {
                        if ep.relations.len() == 1 {
                            params.push(QueryValue::Text(ep.relations[0].clone()));
                            where_parts.push(format!("{e_alias}.relation = ?{}", params.len()));
                        } else {
                            let placeholders = in_placeholders(ep.relations.iter().map(|r| {
                                params.push(QueryValue::Text(r.clone()));
                                params.len()
                            }));
                            where_parts.push(format!("{e_alias}.relation IN ({})", placeholders));
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

    if let Some(where_sql) = compile_where_expr(&query.where_clause, &var_to_alias, &mut params)? {
        where_parts.push(where_sql);
    }

    for item in &query.return_items {
        let var = item.variable();
        if let Some((alias, kind)) = var_to_alias.get(var) {
            match item {
                ReturnItem::Property(_, prop) => {
                    let col = property_to_column(prop, kind)?;
                    let output_alias = quote_identifier(&format!("{var}_{prop}"));
                    select_parts.push(format!("{alias}.{col} AS {output_alias}"));
                }
                ReturnItem::Variable(_) => {
                    let columns: &[(&str, &str)] = match kind {
                        VarKind::Node => &[
                            ("id", "id"),
                            ("namespace", "namespace"),
                            ("kind", "kind"),
                            ("entity_type", "entity_type"),
                            ("name", "name"),
                            ("properties", "properties"),
                            ("created_at", "created_at"),
                            ("updated_at", "updated_at"),
                        ],
                        VarKind::ObservationTargetNode => &[
                            ("id", "id"),
                            ("namespace", "namespace"),
                            ("kind", "kind"),
                            ("entity_type", "entity_type"),
                            ("status", "status"),
                            ("content", "content"),
                            ("salience", "salience"),
                            ("properties", "properties"),
                            ("created_at", "created_at"),
                            ("updated_at", "updated_at"),
                            ("referent_kind", "referent_kind"),
                        ],
                        VarKind::EventNode => &[
                            ("id", "id"),
                            ("namespace", "namespace"),
                            ("verb", "verb"),
                            ("substrate", "substrate"),
                            ("actor", "actor"),
                            ("kind", "kind"),
                            ("outcome", "outcome"),
                            ("payload", "payload"),
                            ("created_at", "created_at"),
                        ],
                        VarKind::Edge => &[
                            ("id", "id"),
                            ("source_id", "source"),
                            ("target_id", "target"),
                            ("relation", "relation"),
                            ("weight", "weight"),
                        ],
                    };
                    for (column, suffix) in columns {
                        let output_alias = quote_identifier(&format!("{var}_{suffix}"));
                        select_parts.push(format!("{alias}.{column} AS {output_alias}"));
                    }
                }
            }
        } else {
            return Err(QueryError::Compile(format!(
                "unknown variable '{var}' in RETURN clause"
            )));
        }
    }

    let offset_i64 = i64::try_from(query.offset)
        .map_err(|_| QueryError::InvalidInput("SKIP exceeds i64::MAX".into()))?;
    params.push(QueryValue::Integer(offset_i64));
    let offset_param = params.len();

    let (limit, truncation_check) = effective_limit(query.limit, query.offset, opts.max_limit);
    let limit_i64 = i64::try_from(limit)
        .map_err(|_| QueryError::InvalidInput("limit exceeds i64::MAX".into()))?;
    params.push(QueryValue::Integer(limit_i64));
    let limit_param = params.len();

    let sql = format!(
        "SELECT {} FROM {} {} WHERE {} ORDER BY {} LIMIT ?{} OFFSET ?{}",
        select_parts.join(", "),
        from_parts.join(", "),
        join_parts.join(" "),
        where_parts.join(" AND "),
        order_parts.join(", "),
        limit_param,
        offset_param,
    );

    Ok(CompiledQuery {
        sql,
        params,
        return_vars: query.return_items.clone(),
        warnings: Vec::new(),
        truncation_check,
    })
}

/// Compiles a `WhereExpr` while preserving its boolean grouping.
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

    let col_expr = where_property_expression(&cond.property, kind, alias)?;

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
            let escaped = khive_types::escape_like_literal(value);
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
                .map(|value| bind_condition_value(value, params))
                .collect::<Result<Vec<_>, _>>()?;
            let collate = if has_string { " COLLATE NOCASE" } else { "" };
            Ok(format!(
                "{col_expr}{collate} IN ({})",
                in_placeholders(placeholders)
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
        CompareOp::IsNull => {
            if !matches!(cond.value, ConditionValue::Null) {
                return Err(QueryError::Validation(
                    "IS NULL does not accept a value".into(),
                ));
            }
            Ok(format!("{col_expr} IS NULL"))
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
                | CompareOp::IsNotNull
                | CompareOp::IsNull => unreachable!(),
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

/// Rejects OR subtrees that cannot be preserved across CTE endpoint phases.
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

    let col_expr = where_property_expression(&cond.property, &VarKind::Node, col_alias)?;

    let sql = compile_condition_predicate(&col_expr, cond, params)?;
    Ok((sql, col_alias))
}

/// Routes variable-length predicates to the start or end CTE phase.
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
            // The prior spanning check guarantees both branches use one endpoint.
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

/// Compiles one endpoint-local expression and returns its CTE alias.
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

/// Compiles one variable-length edge to a recursive CTE.
fn compile_variable_length(
    query: &GqlQuery,
    opts: &CompileOptions,
    mailbox: Option<&QueryMailboxScope>,
) -> Result<CompiledQuery, QueryError> {
    let mut params: Vec<QueryValue> = Vec::new();
    let mailbox = mailbox.map(|scope| MailboxLowering::bind(scope, &mut params));
    let mailbox = mailbox.as_ref();
    let mut var_to_alias: std::collections::HashMap<String, (String, VarKind)> =
        std::collections::HashMap::new();

    // Reject extra elements rather than silently dropping part of the pattern.
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

    // event_observations has no recursive path structure.
    if edge.relations.iter().any(|r| is_synthetic(r)) {
        return Err(QueryError::Unsupported(
            "synthetic observed_as_* edges cannot be variable-length; \
             use a fixed-length edge pattern instead"
                .into(),
        ));
    }

    let max_depth = edge.max_hops.min(MAX_DEPTH);
    let min_depth = edge.min_hops;

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

    let mut relation_condition = String::new();
    if !edge.relations.is_empty() {
        if edge.relations.len() == 1 {
            params.push(QueryValue::Text(edge.relations[0].clone()));
            relation_condition = format!(" AND e.relation = ?{}", params.len());
        } else {
            let placeholders = in_placeholders(edge.relations.iter().map(|r| {
                params.push(QueryValue::Text(r.clone()));
                params.len()
            }));
            relation_condition = format!(" AND e.relation IN ({})", placeholders);
        }
    }

    let e_ns_filter = namespace_filter("e", opts, &mut params);

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

    // Filter every intermediate node so paths cannot cross deleted or out-of-scope rows.
    let next_node_ns_filter = namespace_filter("next_node", opts, &mut params);

    let max_depth_i64 = i64::try_from(max_depth)
        .map_err(|_| QueryError::InvalidInput("max_depth exceeds i64::MAX".into()))?;
    params.push(QueryValue::Integer(max_depth_i64));
    let depth_param = params.len();

    // End filters require `r` even when the end variable is not projected.
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

    reject_or_spanning_endpoints(&query.where_clause, start, end)?;

    if let Some(where_sql) = compile_variable_length_where(
        &query.where_clause,
        start.variable.as_deref(),
        end.variable.as_deref(),
        &mut params,
        &mut start_conditions,
        &mut end_conditions,
    )? {
        // Keep the defensive fallback if a future expression has no endpoint binding.
        start_conditions.push(where_sql);
    }

    if min_depth > 0 {
        let min_depth_i64 = i64::try_from(min_depth)
            .map_err(|_| QueryError::InvalidInput("min_depth exceeds i64::MAX".into()))?;
        params.push(QueryValue::Integer(min_depth_i64));
        end_conditions.push(format!("t.depth >= ?{}", params.len()));
    }

    let offset_i64 = i64::try_from(query.offset)
        .map_err(|_| QueryError::InvalidInput("SKIP exceeds i64::MAX".into()))?;
    params.push(QueryValue::Integer(offset_i64));
    let offset_param = params.len();

    let (limit, truncation_check) = effective_limit(query.limit, query.offset, opts.max_limit);
    let limit_i64 = i64::try_from(limit)
        .map_err(|_| QueryError::InvalidInput("limit exceeds i64::MAX".into()))?;
    params.push(QueryValue::Integer(limit_i64));
    let limit_param = params.len();

    if let Some(ref var) = start.variable {
        var_to_alias.insert(var.clone(), ("s".to_string(), VarKind::Node));
    }
    if let Some(ref var) = end.variable {
        var_to_alias.insert(var.clone(), ("r".to_string(), VarKind::Node));
    }
    if let Some(ref var) = edge.variable {
        var_to_alias.insert(var.clone(), ("e".to_string(), VarKind::Edge));
    }

    let mut select_parts: Vec<String> = Vec::new();
    let mut projection_aliases: Vec<String> = Vec::new();
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
                        let output_alias = quote_identifier(&format!("{var}_{prop}"));
                        select_parts.push(format!("{tbl}.{col} AS {output_alias}"));
                        projection_aliases.push(output_alias);
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
                        let output_alias = quote_identifier(&format!("{var}_{prop}"));
                        select_parts.push(format!("t.{col} AS {output_alias}"));
                        projection_aliases.push(output_alias);
                    }
                }
                ReturnItem::Variable(_) => match kind {
                    VarKind::Node => {
                        let tbl = if start.variable.as_deref() == Some(var) {
                            has_start = true;
                            "s"
                        } else {
                            "r"
                        };
                        for column in [
                            "id",
                            "namespace",
                            "kind",
                            "entity_type",
                            "name",
                            "properties",
                            "created_at",
                            "updated_at",
                        ] {
                            let output_alias = quote_identifier(&format!("{var}_{column}"));
                            select_parts.push(format!("{tbl}.{column} AS {output_alias}"));
                            projection_aliases.push(output_alias);
                        }
                    }
                    VarKind::EventNode | VarKind::ObservationTargetNode => {
                        return Err(QueryError::Unsupported(
                            "synthetic observed_as_* edges cannot be used in variable-length \
                             patterns; use a fixed-length edge pattern instead"
                                .into(),
                        ));
                    }
                    VarKind::Edge => {
                        for (column, suffix) in [
                            ("via_edge", "id"),
                            ("via_relation", "relation"),
                            ("via_weight", "weight"),
                        ] {
                            let output_alias = quote_identifier(&format!("{var}_{suffix}"));
                            select_parts.push(format!("t.{column} AS {output_alias}"));
                            projection_aliases.push(output_alias);
                        }
                    }
                },
            }
        } else {
            return Err(QueryError::Compile(format!(
                "unknown variable '{var}' in RETURN clause"
            )));
        }
    }

    let depth_alias = quote_identifier("_depth");
    let total_weight_alias = quote_identifier("_total_weight");
    select_parts.push(format!("t.depth AS {depth_alias}"));
    select_parts.push(format!("t.total_weight AS {total_weight_alias}"));

    // This path uses SELECT DISTINCT, so ordering by hidden path identities can
    // pick an arbitrary representative for collapsed rows. Ordering by every
    // projected alias instead gives a total order over the rows that survive
    // DISTINCT while retaining depth/weight as the leading traversal order.
    // Keep aliases as data: public AST names may contain SQL delimiters.
    projection_aliases.retain(|alias| alias != &depth_alias && alias != &total_weight_alias);
    let projection_order = projection_aliases.join(", ");
    let projection_order_suffix = if projection_order.is_empty() {
        String::new()
    } else {
        format!(", {projection_order}")
    };

    // `r` is required by end filters; `s` is needed only for a start projection.
    let join_start = if has_start {
        "JOIN primary_nodes s ON s.id = t.start_id"
    } else {
        ""
    };
    let join_end = "JOIN primary_nodes r ON r.id = t.current_id";

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
             JOIN {edges} ON {seed_join} AND e.deleted_at IS NULL{e_ns_filter}{relation_condition} \
             JOIN primary_nodes next_node ON next_node.id = ({seed_next}) \
                    AND next_node.deleted_at IS NULL{next_node_ns_and} \
             WHERE {start_where} \
             UNION ALL \
             SELECT t.start_id, {recurse_next}, t.depth + 1, \
                    t.path || ',' || {recurse_next}, \
                    t.total_weight + e.weight, \
                    e.id, e.relation, e.weight \
             FROM traverse t CROSS JOIN {edges} \
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
         ORDER BY {depth_alias}, {total_weight_alias} DESC{projection_order_suffix} \
         LIMIT ?{limit_param} OFFSET ?{offset_param}",
        primary_nodes = primary_node_sql(mailbox),
        edges = edge_source("e", mailbox),
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
        projection_order_suffix = projection_order_suffix,
        limit_param = limit_param,
        offset_param = offset_param,
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
    /// Synthetic observation source backed by `events`.
    EventNode,
    /// Synthetic observation target backed by an entity/note referent.
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
/// Projection columns for entity/note observation targets.
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
/// Projection columns for synthetic event sources.
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
const NODE_WHERE_COLUMNS: &[&str] = &[
    "id",
    "name",
    "kind",
    "entity_type",
    "description",
    "created_at",
    "updated_at",
];
const OBSERVATION_TARGET_WHERE_COLUMNS: &[&str] = &[
    "id",
    "kind",
    "entity_type",
    "status",
    "name",
    "content",
    "salience",
    "decay_factor",
    "created_at",
    "updated_at",
    "referent_kind",
];
const EVENT_WHERE_COLUMNS: &[&str] = &[
    "id",
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
const EDGE_WHERE_COLUMNS: &[&str] = &["relation", "weight"];

fn property_columns(kind: &VarKind) -> (&'static [&'static str], &'static str) {
    match kind {
        VarKind::Node => (NODE_COLUMNS, "node"),
        VarKind::ObservationTargetNode => (OBSERVATION_TARGET_COLUMNS, "observation target"),
        VarKind::EventNode => (EVENT_COLUMNS, "event"),
        VarKind::Edge => (EDGE_COLUMNS, "edge"),
    }
}

fn property_to_column<'a>(prop: &'a str, kind: &VarKind) -> Result<&'a str, QueryError> {
    let (valid, kind_name) = property_columns(kind);
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

fn where_property_columns(kind: &VarKind) -> (&'static [&'static str], &'static str) {
    match kind {
        VarKind::Node => (NODE_WHERE_COLUMNS, "node"),
        VarKind::ObservationTargetNode => (OBSERVATION_TARGET_WHERE_COLUMNS, "observation target"),
        VarKind::EventNode => (EVENT_WHERE_COLUMNS, "event"),
        VarKind::Edge => (EDGE_WHERE_COLUMNS, "edge"),
    }
}

fn where_property_expression(
    property: &PropertyRef,
    kind: &VarKind,
    alias: &str,
) -> Result<String, QueryError> {
    let (valid, kind_name) = where_property_columns(kind);
    match property {
        PropertyRef::Field(field) if valid.contains(&field.as_str()) => {
            Ok(format!("{alias}.{field}"))
        }
        PropertyRef::Field(field) if field == "properties" => Err(QueryError::Compile(
            "'properties' is reserved as the JSON path root in WHERE clauses; \
             use 'properties.<path>'"
                .into(),
        )),
        PropertyRef::Field(field) => Err(QueryError::Compile(format!(
            "unknown {kind_name} field '{field}' in WHERE clause. Valid fields: {}; \
             JSON properties must use 'properties.<path>'",
            valid.join(", ")
        ))),
        PropertyRef::JsonPath(path) => {
            let (projection_columns, _) = property_columns(kind);
            if !projection_columns.contains(&"properties") {
                return Err(QueryError::Compile(format!(
                    "{kind_name} variables do not expose JSON properties in WHERE clauses. \
                     Valid fields: {}",
                    valid.join(", ")
                )));
            }
            if path.is_empty() {
                return Err(QueryError::Compile(
                    "JSON property path cannot be empty; use 'properties.<path>'".into(),
                ));
            }
            if let Some(segment) = path.iter().find(|segment| {
                segment.is_empty() || !segment.chars().all(|ch| ch.is_alphanumeric() || ch == '_')
            }) {
                return Err(QueryError::Compile(format!(
                    "invalid JSON property path segment '{segment}'; \
                     path segments must be identifiers"
                )));
            }
            Ok(format!(
                "json_extract({alias}.properties, '$.{}')",
                path.join(".")
            ))
        }
    }
}

// Inline tests exercise private compiler phases without widening the public API.
#[cfg(test)]
#[path = "sql_tests.rs"]
mod tests;
