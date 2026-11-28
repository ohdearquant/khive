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
/// An explicit query-text `LIMIT` bounds rows after `SKIP` (ADR-008 Amendment 3).
/// Limits at or below `max_limit` are terminal without a sentinel. Larger or absent
/// limits request `max_limit + 1` rows so the runtime can detect another page.
/// See `crates/khive-query/docs/api/sql-compilation.md` for lifecycle details.
fn effective_limit(
    requested_limit: Option<usize>,
    max_limit: usize,
) -> (usize, Option<TruncationCheck>) {
    match requested_limit {
        Some(limit) => {
            if limit <= max_limit {
                (limit, None)
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
        if query.pattern.edges().count() > 1 {
            compile_mixed_length(&query, opts, mailbox)?
        } else {
            compile_variable_length(&query, opts, mailbox)?
        }
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

struct NodeCorrelation {
    id: String,
    substrate: String,
}

/// One lowering context owns all block aliases and reuses the caller's bound scope.
struct AbsenceContext<'a> {
    opts: &'a CompileOptions,
    mailbox: Option<&'a MailboxLowering>,
    nodes: std::collections::HashMap<String, NodeCorrelation>,
    next_block: usize,
}

impl<'a> AbsenceContext<'a> {
    fn new(opts: &'a CompileOptions, mailbox: Option<&'a MailboxLowering>) -> Self {
        Self {
            opts,
            mailbox,
            nodes: std::collections::HashMap::new(),
            next_block: 0,
        }
    }

    fn bind_node(&mut self, variable: &str, alias: &str, substrate: String) {
        self.nodes.insert(
            variable.to_owned(),
            NodeCorrelation {
                id: format!("{alias}.id"),
                substrate,
            },
        );
    }

    fn lower(
        &mut self,
        pattern: &MatchPattern,
        params: &mut Vec<QueryValue>,
    ) -> Result<String, QueryError> {
        let block = self.next_block;
        self.next_block += 1;
        let mut sources = Vec::new();
        let mut predicates = Vec::new();
        let mut node_index = 0usize;
        let mut edge_index = 0usize;
        for element in &pattern.elements {
            match element {
                PatternElement::Node(node) => {
                    let alias = format!("absence_{block}_n{node_index}");
                    sources.push(primary_node_source(&alias, self.mailbox));
                    predicates.push(format!("{alias}.deleted_at IS NULL"));
                    let scope = namespace_filter(&alias, self.opts, params);
                    if !scope.is_empty() {
                        predicates.push(scope.trim_start_matches(" AND ").to_owned());
                    }
                    if let Some(kind) = &node.kind {
                        predicates.push(kind_filter_predicate(&alias, kind, params));
                    }
                    if let Some(entity_type) = &node.entity_type {
                        params.push(QueryValue::Text(entity_type.clone()));
                        predicates.push(format!("{alias}.entity_type = ?{}", params.len()));
                    }
                    let mut properties: Vec<_> = node.properties.iter().collect();
                    properties.sort_by_key(|(key, _)| key.as_str());
                    for (key, value) in properties {
                        let text_column = if key == "name" { Some("name") } else { None };
                        predicates.push(compile_property_equality(
                            &alias,
                            key,
                            value,
                            text_column,
                            params,
                        )?);
                    }
                    if let Some(correlation) =
                        node.variable.as_ref().and_then(|v| self.nodes.get(v))
                    {
                        predicates.push(format!("{alias}.id = {}", correlation.id));
                        predicates.push(format!(
                            "{alias}.substrate_kind = {}",
                            correlation.substrate,
                        ));
                    }
                    node_index += 1;
                }
                PatternElement::Edge(edge) => {
                    let alias = format!("absence_{block}_e{edge_index}");
                    let previous = format!("absence_{block}_n{}", node_index - 1);
                    let next = format!("absence_{block}_n{node_index}");
                    sources.push(edge_source(&alias, self.mailbox));
                    predicates.push(format!("{alias}.deleted_at IS NULL"));
                    let scope = namespace_filter(&alias, self.opts, params);
                    if !scope.is_empty() {
                        predicates.push(scope.trim_start_matches(" AND ").to_owned());
                    }
                    let outgoing = format!(
                        "{alias}.source_id = {previous}.id AND {alias}.target_id = {next}.id"
                    );
                    let incoming = format!(
                        "{alias}.target_id = {previous}.id AND {alias}.source_id = {next}.id"
                    );
                    predicates.push(match edge.direction {
                        EdgeDirection::Out => outgoing,
                        EdgeDirection::In => incoming,
                        EdgeDirection::Both => format!("(({outgoing}) OR ({incoming}))"),
                    });
                    if edge.relations.len() == 1 {
                        params.push(QueryValue::Text(edge.relations[0].clone()));
                        predicates.push(format!("{alias}.relation = ?{}", params.len()));
                    } else if !edge.relations.is_empty() {
                        let placeholders = in_placeholders(edge.relations.iter().map(|relation| {
                            params.push(QueryValue::Text(relation.clone()));
                            params.len()
                        }));
                        predicates.push(format!("{alias}.relation IN ({placeholders})"));
                    }
                    edge_index += 1;
                }
            }
        }
        Ok(format!(
            "NOT EXISTS (SELECT 1 FROM {} WHERE {})",
            sources.join(", "),
            predicates.join(" AND "),
        ))
    }
}

fn contains_not_exists(expr: &WhereExpr) -> bool {
    match expr {
        WhereExpr::NotExists(_) => true,
        WhereExpr::And(left, right) | WhereExpr::Or(left, right) => {
            contains_not_exists(left) || contains_not_exists(right)
        }
        WhereExpr::True | WhereExpr::Condition(_) => false,
    }
}

fn absence_needs_final_start(
    expr: &WhereExpr,
    start_var: Option<&str>,
    end_var: Option<&str>,
) -> bool {
    match expr {
        WhereExpr::NotExists(pattern) => {
            pattern.nodes().any(|node| {
                node.variable
                    .as_deref()
                    .is_some_and(|v| Some(v) == start_var)
            }) && pattern
                .nodes()
                .any(|node| node.variable.as_deref().is_some_and(|v| Some(v) == end_var))
        }
        WhereExpr::And(left, right) | WhereExpr::Or(left, right) => {
            absence_needs_final_start(left, start_var, end_var)
                || absence_needs_final_start(right, start_var, end_var)
        }
        WhereExpr::True | WhereExpr::Condition(_) => false,
    }
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
    let mut output_aliases = ProjectionAliases::default();
    let has_counts = has_binding_counts(query);
    let result_mode = query.distinct || has_counts;
    let mut result_values = ResultValues::default();

    let mut node_aliases: Vec<String> = Vec::new();
    let mut var_to_alias: std::collections::HashMap<String, (String, VarKind)> =
        std::collections::HashMap::new();

    // Synthetic endpoints bind different substrate sources (issue #468).
    let (event_source_indices, observation_target_indices) =
        synthetic_endpoint_node_indices(&query.pattern.elements);

    let mut node_idx = 0usize;
    let mut edge_idx = 0usize;

    for pattern in query.patterns() {
        let path_start_node_idx = node_idx;
        for element in &pattern.elements {
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
                    } else if node_idx == path_start_node_idx {
                        // Validation permits only canonical comma components. Keep the
                        // same filtered source on each root before the global identity join.
                        join_parts.push(format!(
                            "CROSS JOIN {}",
                            primary_node_source(&alias, mailbox)
                        ));
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
                        if let Some((first_alias, _)) = var_to_alias.get(var) {
                            // A repeated name across paths denotes a typed record identity.
                            // Retain the first alias for projections and every occurrence's filters.
                            where_parts.push(format!(
                                "{alias}.substrate_kind = {first_alias}.substrate_kind"
                            ));
                            where_parts.push(format!("{alias}.id = {first_alias}.id"));
                        } else {
                            var_to_alias.insert(var.clone(), (alias.clone(), kind));
                        }
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
                                where_parts
                                    .push(format!("{e_alias}.relation IN ({})", placeholders));
                            }
                        }
                    }

                    if let Some(ref var) = ep.variable {
                        let kind = if has_synthetic {
                            VarKind::SyntheticEdge
                        } else {
                            VarKind::Edge
                        };
                        var_to_alias.insert(var.clone(), (e_alias.clone(), kind));
                    }

                    edge_idx += 1;
                }
            }
        }
    }

    let mut absence = AbsenceContext::new(opts, mailbox);
    for (variable, (alias, kind)) in &var_to_alias {
        let substrate = match kind {
            VarKind::Node => format!("{alias}.substrate_kind"),
            VarKind::ObservationTargetNode => format!("{alias}.referent_kind"),
            VarKind::EventNode => "'event'".to_owned(),
            VarKind::Edge | VarKind::SyntheticEdge => continue,
        };
        absence.bind_node(variable, alias, substrate);
    }
    if let Some(where_sql) = compile_where_expr(
        &query.where_clause,
        &var_to_alias,
        &mut params,
        &mut absence,
    )? {
        where_parts.push(where_sql);
    }

    for item in &query.return_items {
        let var = item.variable();
        if let Some((alias, kind)) = var_to_alias.get(var) {
            match item {
                ReturnItem::Count(_) => {
                    // Every fixed binding is mandatory, including synthetic edges
                    // whose exposed id is NULL but whose observation row exists.
                    let expression = "COUNT(1)".to_owned();
                    let output_alias = output_aliases.register(
                        format!("count_{var}"),
                        ProjectionIdentity::Count(var.to_owned()),
                    )?;
                    select_parts.push(select_projection(&expression, &output_alias, result_mode));
                    result_values.count(expression);
                }
                ReturnItem::Path(_, property) => {
                    let (root, path) = return_path_parts(property)?;
                    let expression = return_path_expression(
                        property,
                        kind,
                        &format!("{alias}.{root}"),
                        &mut params,
                    )?;
                    let output_alias = output_aliases.register(
                        format!("{var}_{root}_{}", path.join("_")),
                        ProjectionIdentity::Property(var.to_owned(), property.clone()),
                    )?;
                    select_parts.push(select_projection(&expression, &output_alias, result_mode));
                    result_values.property(var, property.clone(), expression);
                }
                ReturnItem::Property(_, prop) => {
                    let expression = scalar_property_expression(prop, kind, alias)?;
                    let output_alias = output_aliases.register(
                        format!("{var}_{prop}"),
                        ProjectionIdentity::Property(
                            var.to_owned(),
                            PropertyRef::Field(prop.clone()),
                        ),
                    )?;
                    select_parts.push(select_projection(&expression, &output_alias, result_mode));
                    result_values.property(var, PropertyRef::Field(prop.clone()), expression);
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
                        VarKind::Edge | VarKind::SyntheticEdge => &[
                            ("id", "id"),
                            ("source_id", "source"),
                            ("target_id", "target"),
                            ("relation", "relation"),
                            ("weight", "weight"),
                        ],
                    };
                    for (column, suffix) in columns {
                        let output_alias = output_aliases.register(
                            format!("{var}_{suffix}"),
                            ProjectionIdentity::Property(
                                var.to_owned(),
                                PropertyRef::Field((*column).to_owned()),
                            ),
                        )?;
                        let expression = format!("{alias}.{column}");
                        select_parts.push(select_projection(
                            &expression,
                            &output_alias,
                            result_mode,
                        ));
                        result_values.property(
                            var,
                            PropertyRef::Field((*column).to_owned()),
                            expression,
                        );
                    }
                }
            }
        } else {
            return Err(QueryError::Compile(format!(
                "unknown variable '{var}' in RETURN clause"
            )));
        }
    }

    if result_mode {
        order_parts = result_values.order(&query.order_by, &mut params)?;
    } else if !query.order_by.is_empty() {
        let mut requested = Vec::new();
        for item in &query.order_by {
            let (alias, kind) = var_to_alias.get(&item.variable).ok_or_else(|| {
                QueryError::Compile(format!("unknown variable '{}' in ORDER BY", item.variable))
            })?;
            let expression = order_property_expression(&item.property, kind, alias, &mut params)?;
            requested.push(format!(
                "{expression} COLLATE BINARY {}",
                order_direction(item.direction)
            ));
        }
        requested.extend(order_parts);
        order_parts = requested;
    }

    let offset_i64 = i64::try_from(query.offset)
        .map_err(|_| QueryError::InvalidInput("SKIP exceeds i64::MAX".into()))?;
    params.push(QueryValue::Integer(offset_i64));
    let offset_param = params.len();

    let (limit, truncation_check) = effective_limit(query.limit, opts.max_limit);
    let limit_i64 = i64::try_from(limit)
        .map_err(|_| QueryError::InvalidInput("limit exceeds i64::MAX".into()))?;
    params.push(QueryValue::Integer(limit_i64));
    let limit_param = params.len();

    let distinct = if query.distinct { "DISTINCT " } else { "" };
    let group_by = result_values.group_by(has_counts);
    let sql = format!(
        "SELECT {distinct}{} FROM {} {} WHERE {}{group_by} ORDER BY {} LIMIT ?{} OFFSET ?{}",
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
    absence: &mut AbsenceContext<'_>,
) -> Result<Option<String>, QueryError> {
    match expr {
        WhereExpr::True => Ok(None),
        WhereExpr::NotExists(pattern) => absence.lower(pattern, params).map(Some),
        WhereExpr::Condition(cond) => {
            let sql = compile_single_condition(cond, var_to_alias, params)?;
            Ok(Some(sql))
        }
        WhereExpr::And(l, r) => {
            let ls = compile_where_expr(l, var_to_alias, params, absence)?;
            let rs = compile_where_expr(r, var_to_alias, params, absence)?;
            Ok(match (ls, rs) {
                (None, None) => None,
                (Some(s), None) | (None, Some(s)) => Some(s),
                (Some(l), Some(r)) => Some(format!("{l} AND {r}")),
            })
        }
        WhereExpr::Or(l, r) => {
            let ls = compile_where_expr(l, var_to_alias, params, absence)?;
            let rs = compile_where_expr(r, var_to_alias, params, absence)?;
            Ok(match (ls, rs) {
                (None, None) => None,
                (Some(s), None) => Some(format!("({s} OR 1)")),
                (None, Some(s)) => Some(format!("(1 OR {s})")),
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

    let col_expr = where_property_expression(&cond.property, kind, alias, params)?;

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
    edge_var: Option<&str>,
) -> (bool, bool) {
    match expr {
        WhereExpr::True => (false, false),
        WhereExpr::NotExists(pattern) => {
            let uses_end = pattern
                .nodes()
                .any(|node| node.variable.as_deref().is_some_and(|v| Some(v) == end_var));
            let uses_start = pattern.nodes().any(|node| {
                node.variable
                    .as_deref()
                    .is_some_and(|v| Some(v) == start_var)
            });
            // A mixed-correlated atom is indivisible and belongs to the final row.
            (uses_start && !uses_end, uses_end)
        }
        WhereExpr::Condition(c) => {
            let is_start = start_var == Some(c.variable.as_str());
            let is_end = end_var == Some(c.variable.as_str())
                || (edge_var == Some(c.variable.as_str())
                    && matches!(&c.property, PropertyRef::MetadataPath(_)));
            (is_start, is_end)
        }
        WhereExpr::And(l, r) | WhereExpr::Or(l, r) => {
            let (ls, le) = expr_endpoint_set(l, start_var, end_var, edge_var);
            let (rs, re) = expr_endpoint_set(r, start_var, end_var, edge_var);
            (ls || rs, le || re)
        }
    }
}

/// Rejects OR subtrees spanning the start phase and final node/edge phase.
fn reject_or_spanning_endpoints(
    expr: &WhereExpr,
    start: &NodePattern,
    end: &NodePattern,
    edge: &EdgePattern,
) -> Result<(), QueryError> {
    let start_var = start.variable.as_deref();
    let end_var = end.variable.as_deref();
    let edge_var = edge.variable.as_deref();
    reject_or_spanning_impl(expr, start_var, end_var, edge_var)
}

fn reject_or_spanning_impl(
    expr: &WhereExpr,
    start_var: Option<&str>,
    end_var: Option<&str>,
    edge_var: Option<&str>,
) -> Result<(), QueryError> {
    match expr {
        WhereExpr::True | WhereExpr::Condition(_) | WhereExpr::NotExists(_) => Ok(()),
        WhereExpr::And(l, r) => {
            reject_or_spanning_impl(l, start_var, end_var, edge_var)?;
            reject_or_spanning_impl(r, start_var, end_var, edge_var)
        }
        WhereExpr::Or(l, r) => {
            let (l_start, l_end) = expr_endpoint_set(l, start_var, end_var, edge_var);
            let (r_start, r_end) = expr_endpoint_set(r, start_var, end_var, edge_var);
            let spans_start = l_start || r_start;
            let spans_end = l_end || r_end;
            if spans_start && spans_end {
                return Err(QueryError::Unsupported(
                    "WHERE clauses that span the start and final row in a variable-length pattern \
                     are not yet supported; rewrite as separate queries or restrict each \
                     OR branch to one phase"
                        .into(),
                ));
            }
            reject_or_spanning_impl(l, start_var, end_var, edge_var)?;
            reject_or_spanning_impl(r, start_var, end_var, edge_var)
        }
    }
}

fn compile_var_len_condition(
    cond: &Condition,
    start_var: Option<&str>,
    end_var: Option<&str>,
    edge_var: Option<&str>,
    params: &mut Vec<QueryValue>,
) -> Result<(String, &'static str), QueryError> {
    if edge_var == Some(cond.variable.as_str()) {
        if let PropertyRef::MetadataPath(path) = &cond.property {
            let expression = bound_json_path_expression("t.via_metadata", path, params)?;
            // Final-edge predicates share the final node's phase, not its SQL alias.
            return Ok((compile_condition_predicate(&expression, cond, params)?, "r"));
        }
    }
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

    let col_expr = where_property_expression(&cond.property, &VarKind::Node, col_alias, params)?;

    let sql = compile_condition_predicate(&col_expr, cond, params)?;
    Ok((sql, col_alias))
}

fn compile_variable_length_absence(
    pattern: &MatchPattern,
    end_var: Option<&str>,
    params: &mut Vec<QueryValue>,
    absence: &mut AbsenceContext<'_>,
) -> Result<(String, &'static str), QueryError> {
    let final_phase = pattern
        .nodes()
        .any(|node| node.variable.as_deref().is_some_and(|v| Some(v) == end_var));
    Ok((
        absence.lower(pattern, params)?,
        if final_phase { "r" } else { "s" },
    ))
}

/// Routes variable-length predicates to the start or end CTE phase.
fn compile_variable_length_where(
    expr: &WhereExpr,
    (start_var, end_var, edge_var): (Option<&str>, Option<&str>, Option<&str>),
    params: &mut Vec<QueryValue>,
    start_conditions: &mut Vec<String>,
    end_conditions: &mut Vec<String>,
    absence: &mut AbsenceContext<'_>,
) -> Result<Option<String>, QueryError> {
    match expr {
        WhereExpr::True => Ok(None),
        WhereExpr::NotExists(pattern) => {
            let (sql, alias) = compile_variable_length_absence(pattern, end_var, params, absence)?;
            if alias == "s" {
                start_conditions.push(sql);
            } else {
                end_conditions.push(sql);
            }
            Ok(None)
        }
        WhereExpr::Condition(cond) => {
            let (sql, alias) =
                compile_var_len_condition(cond, start_var, end_var, edge_var, params)?;
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
                (start_var, end_var, edge_var),
                params,
                start_conditions,
                end_conditions,
                absence,
            )?;
            compile_variable_length_where(
                r,
                (start_var, end_var, edge_var),
                params,
                start_conditions,
                end_conditions,
                absence,
            )?;
            Ok(None)
        }
        WhereExpr::Or(l, r) => {
            // The prior spanning check guarantees both branches use one endpoint.
            let l_sql = compile_variable_length_where_to_sql(
                l, start_var, end_var, edge_var, params, absence,
            )?;
            let r_sql = compile_variable_length_where_to_sql(
                r, start_var, end_var, edge_var, params, absence,
            )?;
            match (l_sql, r_sql) {
                (None, None) => {}
                (Some((ls, la)), None) => {
                    let combined = format!("({ls} OR 1)");
                    if la == "s" {
                        start_conditions.push(combined);
                    } else {
                        end_conditions.push(combined);
                    }
                }
                (None, Some((rs, ra))) => {
                    let combined = format!("(1 OR {rs})");
                    if ra == "s" {
                        start_conditions.push(combined);
                    } else {
                        end_conditions.push(combined);
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
    edge_var: Option<&str>,
    params: &mut Vec<QueryValue>,
    absence: &mut AbsenceContext<'_>,
) -> Result<Option<(String, &'static str)>, QueryError> {
    match expr {
        WhereExpr::True => Ok(None),
        WhereExpr::NotExists(pattern) => {
            compile_variable_length_absence(pattern, end_var, params, absence).map(Some)
        }
        WhereExpr::Condition(cond) => {
            let (sql, alias) =
                compile_var_len_condition(cond, start_var, end_var, edge_var, params)?;
            Ok(Some((sql, alias)))
        }
        WhereExpr::And(l, r) => {
            let ls = compile_variable_length_where_to_sql(
                l, start_var, end_var, edge_var, params, absence,
            )?;
            let rs = compile_variable_length_where_to_sql(
                r, start_var, end_var, edge_var, params, absence,
            )?;
            Ok(match (ls, rs) {
                (None, None) => None,
                (Some(s), None) | (None, Some(s)) => Some(s),
                (Some((lsql, la)), Some((rsql, _))) => Some((format!("{lsql} AND {rsql}"), la)),
            })
        }
        WhereExpr::Or(l, r) => {
            let ls = compile_variable_length_where_to_sql(
                l, start_var, end_var, edge_var, params, absence,
            )?;
            let rs = compile_variable_length_where_to_sql(
                r, start_var, end_var, edge_var, params, absence,
            )?;
            Ok(match (ls, rs) {
                (None, None) => None,
                (Some((sql, alias)), None) => Some((format!("({sql} OR 1)"), alias)),
                (None, Some((sql, alias))) => Some((format!("(1 OR {sql})"), alias)),
                (Some((lsql, la)), Some((rsql, _))) => Some((format!("({lsql} OR {rsql})"), la)),
            })
        }
    }
}

fn mixed_node_conditions(
    node: &NodePattern,
    alias: &str,
    opts: &CompileOptions,
    params: &mut Vec<QueryValue>,
) -> Result<Vec<String>, QueryError> {
    let mut conditions = vec![format!("{alias}.deleted_at IS NULL")];
    let namespace = namespace_filter(alias, opts, params);
    if !namespace.is_empty() {
        conditions.push(namespace.trim_start_matches(" AND ").to_owned());
    }
    if let Some(kind) = &node.kind {
        conditions.push(kind_filter_predicate(alias, kind, params));
    }
    if let Some(entity_type) = &node.entity_type {
        params.push(QueryValue::Text(entity_type.clone()));
        conditions.push(format!("{alias}.entity_type = ?{}", params.len()));
    }
    let mut properties: Vec<_> = node.properties.iter().collect();
    properties.sort_by_key(|(key, _)| key.as_str());
    for (key, value) in properties {
        let text_column = if key == "name" { Some("name") } else { None };
        conditions.push(compile_property_equality(
            alias,
            key,
            value,
            text_column,
            params,
        )?);
    }
    Ok(conditions)
}

fn mixed_segment_cte(
    index: usize,
    start: &NodePattern,
    edge: &EdgePattern,
    carries_metadata: bool,
    opts: &CompileOptions,
    mailbox: Option<&MailboxLowering>,
    params: &mut Vec<QueryValue>,
) -> Result<String, QueryError> {
    let start_conditions = mixed_node_conditions(start, "s", opts, params)?;
    let edge_namespace = namespace_filter("e", opts, params);
    let next_namespace = namespace_filter("next_node", opts, params);
    let relation = if edge.relations.is_empty() {
        String::new()
    } else {
        let placeholders = in_placeholders(edge.relations.iter().map(|relation| {
            params.push(QueryValue::Text(relation.clone()));
            params.len()
        }));
        format!(" AND e.relation IN ({placeholders})")
    };
    params.push(QueryValue::Integer(i64::try_from(edge.max_hops).map_err(
        |_| QueryError::InvalidInput("max_depth exceeds i64::MAX".into()),
    )?));
    let maximum = params.len();
    let (edge_join, next_id) = match edge.direction {
        EdgeDirection::Out => ("e.source_id = t.current_id", "e.target_id"),
        EdgeDirection::In => ("e.target_id = t.current_id", "e.source_id"),
        EdgeDirection::Both => (
            "(e.source_id = t.current_id OR e.target_id = t.current_id)",
            "CASE WHEN e.source_id = t.current_id THEN e.target_id ELSE e.source_id END",
        ),
    };
    let (metadata_column, metadata_seed, metadata_value) = if carries_metadata {
        (", metadata", ", NULL", ", e.metadata")
    } else {
        ("", "", "")
    };
    // Each segment owns its typed zero seed and visited path. UNION ALL keeps
    // distinct routes which later converge on the same final edge countable.
    Ok(format!(
        "segment_{index}(start_id, current_id, start_substrate, current_substrate, \
         depth, path, total_weight, id, relation, weight{metadata_column}) AS (\
             SELECT s.id, s.id, s.substrate_kind, s.substrate_kind, 0, s.id, 0.0, \
                    NULL, NULL, NULL{metadata_seed} \
             FROM primary_nodes s WHERE {start_where} \
             UNION ALL \
             SELECT t.start_id, {next_id}, t.start_substrate, next_node.substrate_kind, \
                    t.depth + 1, t.path || ',' || {next_id}, t.total_weight + e.weight, \
                    e.id, e.relation, e.weight{metadata_value} \
             FROM segment_{index} t CROSS JOIN {edges} \
                 ON {edge_join} AND e.deleted_at IS NULL{edge_namespace}{relation} \
             JOIN primary_nodes next_node ON next_node.id = ({next_id}) \
                 AND next_node.deleted_at IS NULL{next_namespace} \
             WHERE t.depth < ?{maximum} \
               AND (t.depth = 0 OR (',' || t.path || ',') NOT LIKE '%,' || {next_id} || ',%')\
         )",
        start_where = start_conditions.join(" AND "),
        edges = edge_source("e", mailbox),
    ))
}

fn compile_mixed_length(
    query: &GqlQuery,
    opts: &CompileOptions,
    mailbox: Option<&QueryMailboxScope>,
) -> Result<CompiledQuery, QueryError> {
    let mut params = Vec::new();
    let mailbox = mailbox.map(|scope| MailboxLowering::bind(scope, &mut params));
    let mailbox = mailbox.as_ref();
    let nodes: Vec<_> = query.pattern.nodes().collect();
    let edges: Vec<_> = query.pattern.edges().collect();
    let mut ctes = vec![format!("primary_nodes AS ({})", primary_node_sql(mailbox))];
    let mut joins = Vec::new();
    let mut conditions = Vec::new();
    let mut var_to_alias = std::collections::HashMap::new();
    let mut variable_edges = std::collections::HashSet::new();
    let mut depths = Vec::new();
    let mut weights = Vec::new();

    for (index, node) in nodes.iter().enumerate() {
        let alias = format!("n{index}");
        conditions.extend(mixed_node_conditions(node, &alias, opts, &mut params)?);
        if let Some(variable) = &node.variable {
            var_to_alias.insert(variable.clone(), (alias, VarKind::Node));
        }
    }
    for (index, edge) in edges.iter().enumerate() {
        let previous = format!("n{index}");
        let next = format!("n{}", index + 1);
        let alias = format!("e{index}");
        if edge.min_hops != 1 || edge.max_hops != 1 {
            let carries_metadata = edge.variable.as_deref().is_some_and(|variable| {
                query.return_items.iter().any(|item| {
                    matches!(item, ReturnItem::Property(var, field)
                        if var == variable && field == "metadata")
                        || matches!(item, ReturnItem::Path(var, PropertyRef::MetadataPath(_))
                            if var == variable)
                }) || query.where_clause.conditions().any(|condition| {
                    condition.variable == variable
                        && matches!(&condition.property, PropertyRef::MetadataPath(_))
                }) || query.order_by.iter().any(|item| {
                    item.variable == variable
                        && matches!(&item.property, PropertyRef::MetadataPath(_))
                })
            });
            ctes.push(mixed_segment_cte(
                index,
                nodes[index],
                edge,
                carries_metadata,
                opts,
                mailbox,
                &mut params,
            )?);
            joins.push(format!(
                "JOIN segment_{index} {alias} ON {alias}.start_id = {previous}.id \
                 AND {alias}.start_substrate = {previous}.substrate_kind"
            ));
            joins.push(format!(
                "JOIN primary_nodes {next} ON {next}.id = {alias}.current_id \
                 AND {next}.substrate_kind = {alias}.current_substrate"
            ));
            if edge.min_hops > 0 {
                params.push(QueryValue::Integer(i64::try_from(edge.min_hops).map_err(
                    |_| QueryError::InvalidInput("min_depth exceeds i64::MAX".into()),
                )?));
                conditions.push(format!("{alias}.depth >= ?{}", params.len()));
            }
            depths.push(format!("{alias}.depth"));
            weights.push(format!("{alias}.total_weight"));
            if let Some(variable) = &edge.variable {
                variable_edges.insert(variable.clone());
            }
        } else {
            let (edge_join, next_id) = match edge.direction {
                EdgeDirection::Out => (
                    format!("{alias}.source_id = {previous}.id"),
                    format!("{alias}.target_id"),
                ),
                EdgeDirection::In => (
                    format!("{alias}.target_id = {previous}.id"),
                    format!("{alias}.source_id"),
                ),
                EdgeDirection::Both => (
                    format!("({alias}.source_id = {previous}.id OR {alias}.target_id = {previous}.id)"),
                    format!("CASE WHEN {alias}.source_id = {previous}.id THEN {alias}.target_id ELSE {alias}.source_id END"),
                ),
            };
            let namespace = namespace_filter(&alias, opts, &mut params);
            joins.push(format!(
                "JOIN {} ON {edge_join} AND {alias}.deleted_at IS NULL{namespace}",
                edge_source(&alias, mailbox),
            ));
            joins.push(format!(
                "JOIN primary_nodes {next} ON {next}.id = ({next_id})"
            ));
            if !edge.relations.is_empty() {
                let placeholders = in_placeholders(edge.relations.iter().map(|relation| {
                    params.push(QueryValue::Text(relation.clone()));
                    params.len()
                }));
                conditions.push(format!("{alias}.relation IN ({placeholders})"));
            }
        }
        if let Some(variable) = &edge.variable {
            var_to_alias.insert(variable.clone(), (alias, VarKind::Edge));
        }
    }

    for condition in query.where_clause.conditions() {
        if variable_edges.contains(&condition.variable)
            && !matches!(&condition.property, PropertyRef::MetadataPath(_))
        {
            return Err(QueryError::Compile(format!(
                "variable-length edge '{}' in WHERE supports only metadata.<path> predicates",
                condition.variable,
            )));
        }
    }
    let mut absence = AbsenceContext::new(opts, mailbox);
    for (variable, (alias, kind)) in &var_to_alias {
        if *kind == VarKind::Node {
            absence.bind_node(variable, alias, format!("{alias}.substrate_kind"));
        }
    }
    if let Some(predicate) = compile_where_expr(
        &query.where_clause,
        &var_to_alias,
        &mut params,
        &mut absence,
    )? {
        conditions.push(predicate);
    }

    let has_counts = has_binding_counts(query);
    let result_mode = query.distinct || has_counts;
    let mut output_aliases = ProjectionAliases::default();
    let mut result_values = ResultValues::default();
    let mut selections = Vec::new();
    for item in &query.return_items {
        let variable = item.variable();
        let (alias, kind) = var_to_alias.get(variable).ok_or_else(|| {
            QueryError::Compile(format!("unknown variable '{variable}' in RETURN clause"))
        })?;
        let is_variable_edge = variable_edges.contains(variable);
        let projections: Vec<(String, PropertyRef, String)> = match item {
            ReturnItem::Count(_) => {
                let expression = if is_variable_edge {
                    format!("COUNT({alias}.id)")
                } else {
                    "COUNT(1)".to_owned()
                };
                let output = output_aliases.register(
                    format!("count_{variable}"),
                    ProjectionIdentity::Count(variable.to_owned()),
                )?;
                selections.push(select_projection(&expression, &output, result_mode));
                result_values.count(expression);
                continue;
            }
            ReturnItem::Path(_, property) => {
                let (root, path) = return_path_parts(property)?;
                let expression = return_path_expression(
                    property,
                    kind,
                    &format!("{alias}.{root}"),
                    &mut params,
                )?;
                vec![(
                    format!("{variable}_{root}_{}", path.join("_")),
                    property.clone(),
                    expression,
                )]
            }
            ReturnItem::Property(_, property) => {
                if is_variable_edge
                    && !["id", "relation", "weight", "metadata"].contains(&property.as_str())
                {
                    return Err(QueryError::Compile(format!(
                        "unknown variable-length edge property '{property}' in RETURN projection. Valid: id, relation, weight, metadata"
                    )));
                }
                let column = property_to_column(property, kind)?;
                vec![(
                    format!("{variable}_{property}"),
                    PropertyRef::Field(property.clone()),
                    format!("{alias}.{column}"),
                )]
            }
            ReturnItem::Variable(_) => {
                let columns: &[(&str, &str)] = if *kind == VarKind::Node {
                    &[
                        ("id", "id"),
                        ("namespace", "namespace"),
                        ("kind", "kind"),
                        ("entity_type", "entity_type"),
                        ("name", "name"),
                        ("properties", "properties"),
                        ("created_at", "created_at"),
                        ("updated_at", "updated_at"),
                    ]
                } else if is_variable_edge {
                    &[("id", "id"), ("relation", "relation"), ("weight", "weight")]
                } else {
                    &[
                        ("id", "id"),
                        ("source_id", "source"),
                        ("target_id", "target"),
                        ("relation", "relation"),
                        ("weight", "weight"),
                    ]
                };
                columns
                    .iter()
                    .map(|(column, suffix)| {
                        (
                            format!("{variable}_{suffix}"),
                            PropertyRef::Field((*column).to_owned()),
                            format!("{alias}.{column}"),
                        )
                    })
                    .collect()
            }
        };
        for (output, property, expression) in projections {
            let output = output_aliases.register(
                output,
                ProjectionIdentity::Property(variable.to_owned(), property.clone()),
            )?;
            selections.push(select_projection(&expression, &output, result_mode));
            result_values.property(variable, property, expression);
        }
    }

    let group_by = result_values.group_by(has_counts);
    let mut order =
        result_values
            .order(&query.order_by, &mut params)
            .map_err(|error| match error {
                QueryError::Unsupported(_) if !result_mode => QueryError::Unsupported(
                    "mixed variable-length ORDER BY must be derived from RETURN projections".into(),
                ),
                other => other,
            })?;
    if !result_mode {
        let depth = format!("({})", depths.join(" + "));
        let weight = format!("({})", weights.join(" + "));
        let depth_alias = output_aliases.register(
            "_depth".into(),
            ProjectionIdentity::TraversalMetric("depth"),
        )?;
        let weight_alias = output_aliases.register(
            "_total_weight".into(),
            ProjectionIdentity::TraversalMetric("total_weight"),
        )?;
        selections.push(format!("{depth} AS {depth_alias}"));
        selections.push(format!("{weight} AS {weight_alias}"));
        // Requested values lead; metrics precede selected-value ties. Expression
        // ordering avoids SQLite's case-insensitive output-alias resolution.
        order.insert(query.order_by.len(), format!("{weight} DESC"));
        order.insert(query.order_by.len(), depth);
    }
    params.push(QueryValue::Integer(i64::try_from(query.offset).map_err(
        |_| QueryError::InvalidInput("SKIP exceeds i64::MAX".into()),
    )?));
    let offset = params.len();
    let (limit, truncation_check) = effective_limit(query.limit, opts.max_limit);
    params.push(QueryValue::Integer(i64::try_from(limit).map_err(|_| {
        QueryError::InvalidInput("limit exceeds i64::MAX".into())
    })?));
    let limit = params.len();
    let distinct = if query.distinct || !result_mode {
        "DISTINCT "
    } else {
        ""
    };
    let sql = format!(
        "WITH RECURSIVE {} SELECT {distinct}{} FROM primary_nodes n0 {} \
         WHERE {}{group_by} ORDER BY {} LIMIT ?{limit} OFFSET ?{offset}",
        ctes.join(", "),
        selections.join(", "),
        joins.join(" "),
        conditions.join(" AND "),
        order.join(", "),
    );
    Ok(CompiledQuery {
        sql,
        params,
        return_vars: query.return_items.clone(),
        warnings: Vec::new(),
        truncation_check,
    })
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

    let carries_metadata = query.return_items.iter().any(|item| {
        matches!(item, ReturnItem::Property(var, field)
            if edge.variable.as_deref() == Some(var.as_str()) && field == "metadata")
            || matches!(item, ReturnItem::Path(var, PropertyRef::MetadataPath(_))
                if edge.variable.as_deref() == Some(var.as_str()))
    }) || query.where_clause.conditions().any(|condition| {
        edge.variable.as_deref() == Some(condition.variable.as_str())
            && matches!(&condition.property, PropertyRef::MetadataPath(_))
    }) || query.order_by.iter().any(|item| {
        edge.variable.as_deref() == Some(item.variable.as_str())
            && matches!(&item.property, PropertyRef::MetadataPath(_))
    });
    let metadata_column = if carries_metadata {
        ", via_metadata"
    } else {
        ""
    };
    let metadata_value = if carries_metadata { ", e.metadata" } else { "" };

    let max_depth = edge.max_hops.min(MAX_DEPTH);
    let min_depth = edge.min_hops;
    let has_counts = has_binding_counts(query);
    let result_mode = query.distinct || has_counts;
    // Raw path counts cannot tolerate UUID-only endpoint rejoin fanout. The
    // typed seed also serves positive minima, filtered at the final projection.
    let typed_traversal = min_depth == 0 || result_mode || contains_not_exists(&query.where_clause);

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

    reject_or_spanning_endpoints(&query.where_clause, start, end, edge)?;

    let mut absence = AbsenceContext::new(opts, mailbox);
    for (node, alias) in [(start, "s"), (end, "r")] {
        if let Some(variable) = &node.variable {
            absence.bind_node(variable, alias, format!("{alias}.substrate_kind"));
        }
    }
    if let Some(where_sql) = compile_variable_length_where(
        &query.where_clause,
        (
            start.variable.as_deref(),
            end.variable.as_deref(),
            edge.variable.as_deref(),
        ),
        &mut params,
        &mut start_conditions,
        &mut end_conditions,
        &mut absence,
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
    let mut projection_expressions: Vec<String> = Vec::new();
    let has_path_projections = query
        .return_items
        .iter()
        .any(|item| matches!(item, ReturnItem::Path(_, _)));
    let mut output_aliases = ProjectionAliases::default();
    let mut returned_paths: Vec<(String, PropertyRef, String)> = Vec::new();
    let mut returned_columns: std::collections::HashMap<(String, String), String> =
        std::collections::HashMap::new();
    let mut returned_expressions: std::collections::HashMap<(String, String), String> =
        std::collections::HashMap::new();
    let mut result_values = ResultValues::default();
    let mut has_start = absence_needs_final_start(
        &query.where_clause,
        start.variable.as_deref(),
        end.variable.as_deref(),
    );

    for item in &query.return_items {
        let var = item.variable();
        if let Some((_, kind)) = var_to_alias.get(var) {
            match item {
                ReturnItem::Count(_) => {
                    let expression = if *kind == VarKind::Edge {
                        // Count occurrences, not distinct final-edge identities;
                        // the zero-hop path has no final edge and contributes zero.
                        "COUNT(t.via_edge)"
                    } else {
                        "COUNT(1)"
                    }
                    .to_owned();
                    let output_alias = output_aliases.register(
                        format!("count_{var}"),
                        ProjectionIdentity::Count(var.to_owned()),
                    )?;
                    select_parts.push(select_projection(&expression, &output_alias, result_mode));
                    result_values.count(expression);
                }
                ReturnItem::Path(_, property) => {
                    let (root, path) = return_path_parts(property)?;
                    let column = if *kind == VarKind::Node {
                        let table = if start.variable.as_deref() == Some(var) {
                            has_start = true;
                            "s"
                        } else {
                            "r"
                        };
                        format!("{table}.{root}")
                    } else {
                        "t.via_metadata".to_owned()
                    };
                    let expression = return_path_expression(property, kind, &column, &mut params)?;
                    let output_alias = output_aliases.register(
                        format!("{var}_{root}_{}", path.join("_")),
                        ProjectionIdentity::Property(var.to_owned(), property.clone()),
                    )?;
                    select_parts.push(select_projection(&expression, &output_alias, result_mode));
                    result_values.property(var, property.clone(), expression.clone());
                    returned_paths.push((var.to_owned(), property.clone(), expression.clone()));
                    projection_expressions.push(expression);
                    projection_aliases.push(output_alias);
                }
                ReturnItem::Property(_, prop) => {
                    let is_start = start.variable.as_deref() == Some(var);
                    if matches!(
                        kind,
                        VarKind::EventNode
                            | VarKind::ObservationTargetNode
                            | VarKind::SyntheticEdge
                    ) {
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
                        let output_alias = output_aliases.register(
                            format!("{var}_{prop}"),
                            ProjectionIdentity::Property(
                                var.to_owned(),
                                PropertyRef::Field(prop.clone()),
                            ),
                        )?;
                        let expression = format!("{tbl}.{col}");
                        select_parts.push(select_projection(
                            &expression,
                            &output_alias,
                            result_mode,
                        ));
                        result_values.property(
                            var,
                            PropertyRef::Field(prop.clone()),
                            expression.clone(),
                        );
                        returned_expressions
                            .insert((var.to_owned(), prop.to_owned()), expression.clone());
                        projection_expressions.push(expression);
                        returned_columns
                            .insert((var.to_owned(), prop.to_owned()), output_alias.clone());
                        projection_aliases.push(output_alias);
                    } else {
                        let col = match prop.as_str() {
                            "id" => "via_edge",
                            "relation" => "via_relation",
                            "weight" => "via_weight",
                            "metadata" => "via_metadata",
                            _ => {
                                return Err(QueryError::Compile(format!(
                                    "unknown edge property '{prop}' in RETURN projection. \
                                     Valid: id, relation, weight, metadata"
                                )));
                            }
                        };
                        let output_alias = output_aliases.register(
                            format!("{var}_{prop}"),
                            ProjectionIdentity::Property(
                                var.to_owned(),
                                PropertyRef::Field(prop.clone()),
                            ),
                        )?;
                        let expression = format!("t.{col}");
                        select_parts.push(select_projection(
                            &expression,
                            &output_alias,
                            result_mode,
                        ));
                        result_values.property(
                            var,
                            PropertyRef::Field(prop.clone()),
                            expression.clone(),
                        );
                        returned_expressions
                            .insert((var.to_owned(), prop.to_owned()), expression.clone());
                        projection_expressions.push(expression);
                        returned_columns
                            .insert((var.to_owned(), prop.to_owned()), output_alias.clone());
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
                            let output_alias = output_aliases.register(
                                format!("{var}_{column}"),
                                ProjectionIdentity::Property(
                                    var.to_owned(),
                                    PropertyRef::Field(column.to_owned()),
                                ),
                            )?;
                            let expression = format!("{tbl}.{column}");
                            select_parts.push(select_projection(
                                &expression,
                                &output_alias,
                                result_mode,
                            ));
                            result_values.property(
                                var,
                                PropertyRef::Field(column.to_owned()),
                                expression.clone(),
                            );
                            returned_expressions
                                .insert((var.to_owned(), column.to_owned()), expression.clone());
                            projection_expressions.push(expression);
                            returned_columns
                                .insert((var.to_owned(), column.to_owned()), output_alias.clone());
                            projection_aliases.push(output_alias);
                        }
                    }
                    VarKind::EventNode
                    | VarKind::ObservationTargetNode
                    | VarKind::SyntheticEdge => {
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
                            let output_alias = output_aliases.register(
                                format!("{var}_{suffix}"),
                                ProjectionIdentity::Property(
                                    var.to_owned(),
                                    PropertyRef::Field(suffix.to_owned()),
                                ),
                            )?;
                            let expression = format!("t.{column}");
                            select_parts.push(select_projection(
                                &expression,
                                &output_alias,
                                result_mode,
                            ));
                            result_values.property(
                                var,
                                PropertyRef::Field(suffix.to_owned()),
                                expression.clone(),
                            );
                            returned_expressions
                                .insert((var.to_owned(), suffix.to_owned()), expression.clone());
                            projection_expressions.push(expression);
                            returned_columns
                                .insert((var.to_owned(), suffix.to_owned()), output_alias.clone());
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

    let group_by = result_values.group_by(has_counts);
    let result_order = if result_mode {
        result_values
            .order(&query.order_by, &mut params)?
            .join(", ")
    } else {
        let depth_alias = output_aliases.register(
            "_depth".into(),
            ProjectionIdentity::TraversalMetric("depth"),
        )?;
        let total_weight_alias = output_aliases.register(
            "_total_weight".into(),
            ProjectionIdentity::TraversalMetric("total_weight"),
        )?;
        select_parts.push(format!("t.depth AS {depth_alias}"));
        select_parts.push(format!("t.total_weight AS {total_weight_alias}"));

        // This path uses SELECT DISTINCT, so ordering by hidden path identities can
        // pick an arbitrary representative for collapsed rows. Ordering by every
        // projected alias instead gives a total order over the rows that survive
        // DISTINCT while retaining depth/weight as the leading traversal order.
        // Keep aliases as data: public AST names may contain SQL delimiters.
        projection_aliases.retain(|alias| alias != &depth_alias && alias != &total_weight_alias);
        // SQLite resolves output aliases without ASCII case sensitivity. New path
        // queries may have distinct case-preserved labels, so order by their actual
        // selected expressions. Keep legacy SQL unchanged when no path is returned.
        let projection_order = if has_path_projections {
            projection_expressions.join(", ")
        } else {
            projection_aliases.join(", ")
        };
        let (depth_order, total_weight_order) = if has_path_projections {
            ("t.depth", "t.total_weight")
        } else {
            (depth_alias.as_str(), total_weight_alias.as_str())
        };
        let projection_order_suffix = if projection_order.is_empty() {
            String::new()
        } else {
            format!(", {projection_order}")
        };

        let mut requested_order = Vec::new();
        for item in &query.order_by {
            if let Some((_, _, projected)) =
                returned_paths.iter().find(|(variable, property, _)| {
                    variable == &item.variable && property == &item.property
                })
            {
                requested_order.push(format!(
                    "{projected} COLLATE BINARY {}",
                    order_direction(item.direction)
                ));
                continue;
            }
            let root = match &item.property {
                PropertyRef::Field(field) => field.as_str(),
                PropertyRef::JsonPath(_) => "properties",
                PropertyRef::MetadataPath(_) => "metadata",
            };
            let returned_values = if has_path_projections {
                &returned_expressions
            } else {
                &returned_columns
            };
            let projected = returned_values
                .get(&(item.variable.clone(), root.to_owned()))
                .ok_or_else(|| {
                    QueryError::Unsupported(
                        "variable-length ORDER BY must be derived from RETURN projections".into(),
                    )
                })?;
            let expression = match &item.property {
                PropertyRef::Field(_) => projected.clone(),
                PropertyRef::JsonPath(path) | PropertyRef::MetadataPath(path) => {
                    bound_json_path_expression(projected, path, &mut params)?
                }
            };
            requested_order.push(format!(
                "{expression} COLLATE BINARY {}",
                order_direction(item.direction)
            ));
        }
        let requested_order = if requested_order.is_empty() {
            String::new()
        } else {
            format!("{}, ", requested_order.join(", "))
        };

        format!(
            "{requested_order}{depth_order}, {total_weight_order} DESC{projection_order_suffix}"
        )
    };
    let distinct = if query.distinct || !result_mode {
        "DISTINCT "
    } else {
        ""
    };

    let offset_i64 = i64::try_from(query.offset)
        .map_err(|_| QueryError::InvalidInput("SKIP exceeds i64::MAX".into()))?;
    params.push(QueryValue::Integer(offset_i64));
    let offset_param = params.len();

    let (limit, truncation_check) = effective_limit(query.limit, opts.max_limit);
    let limit_i64 = i64::try_from(limit)
        .map_err(|_| QueryError::InvalidInput("limit exceeds i64::MAX".into()))?;
    params.push(QueryValue::Integer(limit_i64));
    let limit_param = params.len();

    // `r` is required by end filters; `s` is needed only for a start projection.
    let join_start = if has_start {
        if typed_traversal {
            "JOIN primary_nodes s ON s.id = t.start_id AND s.substrate_kind = t.start_substrate"
        } else {
            "JOIN primary_nodes s ON s.id = t.start_id"
        }
    } else {
        ""
    };
    let join_end = if typed_traversal {
        "JOIN primary_nodes r ON r.id = t.current_id AND r.substrate_kind = t.current_substrate"
    } else {
        "JOIN primary_nodes r ON r.id = t.current_id"
    };

    let next_node_ns_and = if next_node_ns_filter.is_empty() {
        String::new()
    } else {
        format!(" AND {}", next_node_ns_filter.trim_start_matches(" AND "))
    };

    let sql = if typed_traversal {
        // Seed from nodes, not incident edges: an isolated visible node is a
        // depth-zero path. Carry substrate identity so equal UUIDs in different
        // union members cannot become one another's zero-hop endpoint. The first
        // expansion may take a self-loop, as the positive-depth seed does;
        // subsequent expansions retain the visited-ID cycle guard.
        let metadata_null = if carries_metadata { ", NULL" } else { "" };
        format!(
            "WITH RECURSIVE primary_nodes AS ({primary_nodes}), \
             traverse(start_id, current_id, start_substrate, current_substrate, depth, path, total_weight, via_edge, via_relation, via_weight{metadata_column}) AS (\
                 SELECT s.id, s.id, s.substrate_kind, s.substrate_kind, 0, s.id, 0.0, \
                        NULL, NULL, NULL{metadata_null} \
                 FROM primary_nodes s \
                 WHERE {start_where} \
                 UNION ALL \
                 SELECT t.start_id, {recurse_next}, t.start_substrate, next_node.substrate_kind, \
                        t.depth + 1, t.path || ',' || {recurse_next}, \
                        t.total_weight + e.weight, \
                        e.id, e.relation, e.weight{metadata_value} \
                 FROM traverse t CROSS JOIN {edges} \
                     ON {recurse_join} AND e.deleted_at IS NULL{e_ns_filter}{relation_condition} \
                 JOIN primary_nodes next_node ON next_node.id = ({recurse_next}) \
                        AND next_node.deleted_at IS NULL{next_node_ns_and} \
                 WHERE t.depth < ?{depth_param} \
                   AND (t.depth = 0 OR (',' || t.path || ',') NOT LIKE '%,' || {recurse_next} || ',%') \
             ) \
             SELECT {distinct}{select_cols} \
             FROM traverse t \
             {join_start} {join_end} \
             WHERE {end_where}{group_by} \
             ORDER BY {result_order} \
             LIMIT ?{limit_param} OFFSET ?{offset_param}",
            primary_nodes = primary_node_sql(mailbox),
            edges = edge_source("e", mailbox),
            start_where = start_conditions.join(" AND "),
            select_cols = select_parts.join(", "),
            end_where = end_conditions.join(" AND "),
        )
    } else {
        // Retain the established positive-depth seed and parameter ordering.
        format!(
            "WITH RECURSIVE primary_nodes AS ({primary_nodes}), \
             traverse(start_id, current_id, depth, path, total_weight, via_edge, via_relation, via_weight{metadata_column}) AS (\
                 SELECT s.id, {seed_next}, 1, s.id || ',' || {seed_next}, e.weight, \
                        e.id, e.relation, e.weight{metadata_value} \
                 FROM primary_nodes s \
                 JOIN {edges} ON {seed_join} AND e.deleted_at IS NULL{e_ns_filter}{relation_condition} \
                 JOIN primary_nodes next_node ON next_node.id = ({seed_next}) \
                        AND next_node.deleted_at IS NULL{next_node_ns_and} \
                 WHERE {start_where} \
                 UNION ALL \
                 SELECT t.start_id, {recurse_next}, t.depth + 1, \
                        t.path || ',' || {recurse_next}, \
                        t.total_weight + e.weight, \
                        e.id, e.relation, e.weight{metadata_value} \
                 FROM traverse t CROSS JOIN {edges} \
                     ON {recurse_join} AND e.deleted_at IS NULL{e_ns_filter}{relation_condition} \
                 JOIN primary_nodes next_node ON next_node.id = ({recurse_next}) \
                        AND next_node.deleted_at IS NULL{next_node_ns_and} \
                 WHERE t.depth < ?{depth_param} \
                   AND (',' || t.path || ',') NOT LIKE '%,' || {recurse_next} || ',%' \
             ) \
             SELECT {distinct}{select_cols} \
             FROM traverse t \
             {join_start} {join_end} \
             WHERE {end_where}{group_by} \
             ORDER BY {result_order} \
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
            result_order = result_order,
            limit_param = limit_param,
            offset_param = offset_param,
        )
    };

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
    /// Observation edges have no canonical graph-edge metadata column.
    SyntheticEdge,
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
const EDGE_COLUMNS: &[&str] = &[
    "id",
    "source_id",
    "target_id",
    "relation",
    "weight",
    "metadata",
];
const SYNTHETIC_EDGE_COLUMNS: &[&str] = &["id", "source_id", "target_id", "relation", "weight"];
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
        VarKind::SyntheticEdge => (SYNTHETIC_EDGE_COLUMNS, "edge"),
    }
}

/// Semantic identity excludes parameter positions and generated output labels.
#[derive(PartialEq, Eq)]
enum ProjectionIdentity {
    Property(String, PropertyRef),
    Count(String),
    TraversalMetric(&'static str),
}

fn has_binding_counts(query: &GqlQuery) -> bool {
    query
        .return_items
        .iter()
        .any(|item| matches!(item, ReturnItem::Count(_)))
}

fn select_projection(expression: &str, alias: &str, result_mode: bool) -> String {
    if result_mode {
        // Explicit collation governs DISTINCT as well as grouping and ordering;
        // it does not cast numeric, null or JSON-extracted values to text.
        format!("{expression} COLLATE BINARY AS {alias}")
    } else {
        format!("{expression} AS {alias}")
    }
}

/// Actual selected expressions for distinct/grouped results. Public labels are
/// never SQL lookup keys: SQLite resolves aliases without ASCII case sensitivity.
#[derive(Default)]
struct ResultValues {
    expressions: Vec<String>,
    properties: Vec<(String, PropertyRef, String)>,
}

impl ResultValues {
    fn property(&mut self, variable: &str, property: PropertyRef, expression: String) {
        self.expressions.push(expression.clone());
        self.properties
            .push((variable.to_owned(), property, expression));
    }

    fn count(&mut self, expression: String) {
        self.expressions.push(expression);
    }

    fn group_by(&self, has_counts: bool) -> String {
        if !has_counts || self.properties.is_empty() {
            // A counts-only empty match set must still produce one zero row.
            return String::new();
        }
        let keys = self
            .properties
            .iter()
            .map(|(_, _, expression)| format!("{expression} COLLATE BINARY"))
            .collect::<Vec<_>>();
        format!(" GROUP BY {}", keys.join(", "))
    }

    fn order(
        &self,
        items: &[OrderItem],
        params: &mut Vec<QueryValue>,
    ) -> Result<Vec<String>, QueryError> {
        let mut order = Vec::new();
        for item in items {
            let exact = self.properties.iter().find(|(variable, property, _)| {
                variable == &item.variable && property == &item.property
            });
            let expression = if let Some((_, _, expression)) = exact {
                expression.clone()
            } else {
                let (root, path) = match &item.property {
                    PropertyRef::JsonPath(path) => ("properties", path),
                    PropertyRef::MetadataPath(path) => ("metadata", path),
                    PropertyRef::Field(_) => {
                        return Err(QueryError::Unsupported(
                            "distinct/grouped ORDER BY must be derived from RETURN projections"
                                .into(),
                        ));
                    }
                };
                let container = self.properties.iter().find(|(variable, property, _)| {
                    variable == &item.variable && property == &PropertyRef::Field(root.to_owned())
                });
                let (_, _, expression) = container.ok_or_else(|| {
                    QueryError::Unsupported(
                        "distinct/grouped ORDER BY must be derived from RETURN projections".into(),
                    )
                })?;
                bound_json_path_expression(expression, path, params)?
            };
            order.push(format!(
                "{expression} COLLATE BINARY {}",
                order_direction(item.direction)
            ));
        }
        // Only returned values define ties; identities and traversal metrics
        // which were not requested cannot split groups or DISTINCT results.
        order.extend(
            self.expressions
                .iter()
                .map(|expression| format!("{expression} COLLATE BINARY")),
        );
        Ok(order)
    }
}

#[derive(Default)]
struct ProjectionAliases {
    identities: std::collections::HashMap<String, ProjectionIdentity>,
}

impl ProjectionAliases {
    fn register(
        &mut self,
        alias: String,
        identity: ProjectionIdentity,
    ) -> Result<String, QueryError> {
        if let Some(previous) = self.identities.get(&alias) {
            if previous != &identity {
                return Err(QueryError::Compile(format!(
                    "RETURN projection column name collision: '{alias}' denotes different expressions"
                )));
            }
        } else {
            self.identities.insert(alias.clone(), identity);
        }
        Ok(quote_identifier(&alias))
    }
}

fn return_path_parts(property: &PropertyRef) -> Result<(&'static str, &[String]), QueryError> {
    match property {
        PropertyRef::JsonPath(path) => Ok(("properties", path)),
        PropertyRef::MetadataPath(path) => Ok(("metadata", path)),
        PropertyRef::Field(_) => Err(QueryError::Compile(
            "RETURN paths require a properties or metadata path".into(),
        )),
    }
}

/// The caller supplies a compiler-owned column; the complete JSON path is bound.
fn return_path_expression(
    property: &PropertyRef,
    kind: &VarKind,
    column: &str,
    params: &mut Vec<QueryValue>,
) -> Result<String, QueryError> {
    let (_, path) = return_path_parts(property)?;
    match property {
        PropertyRef::JsonPath(_)
            if !matches!(kind, VarKind::Node | VarKind::ObservationTargetNode) =>
        {
            return Err(QueryError::Compile(
                "RETURN properties paths require a node binding with JSON properties".into(),
            ));
        }
        PropertyRef::MetadataPath(_) if *kind != VarKind::Edge => {
            return Err(QueryError::Compile(
                "metadata paths require a canonical edge binding".into(),
            ));
        }
        _ => {}
    }
    bound_json_path_expression(column, path, params)
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

fn scalar_property_expression(
    prop: &str,
    kind: &VarKind,
    alias: &str,
) -> Result<String, QueryError> {
    let column = property_to_column(prop, kind)?;
    if *kind == VarKind::SyntheticEdge && column == "id" {
        Ok("NULL".to_owned())
    } else {
        Ok(format!("{alias}.{column}"))
    }
}

fn where_property_columns(kind: &VarKind) -> (&'static [&'static str], &'static str) {
    match kind {
        VarKind::Node => (NODE_WHERE_COLUMNS, "node"),
        VarKind::ObservationTargetNode => (OBSERVATION_TARGET_WHERE_COLUMNS, "observation target"),
        VarKind::EventNode => (EVENT_WHERE_COLUMNS, "event"),
        VarKind::Edge => (EDGE_WHERE_COLUMNS, "edge"),
        VarKind::SyntheticEdge => (EDGE_WHERE_COLUMNS, "edge"),
    }
}

fn where_property_expression(
    property: &PropertyRef,
    kind: &VarKind,
    alias: &str,
    params: &mut Vec<QueryValue>,
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
            json_path_expression(&format!("{alias}.properties"), path)
        }
        PropertyRef::MetadataPath(path) => {
            if *kind != VarKind::Edge {
                return Err(QueryError::Compile(
                    "metadata paths require a canonical edge binding".into(),
                ));
            }
            bound_json_path_expression(&format!("{alias}.metadata"), path, params)
        }
    }
}

fn validated_json_path(path: &[String]) -> Result<String, QueryError> {
    if path.is_empty() {
        return Err(QueryError::Compile(
            "JSON property path cannot be empty; use a named path below the JSON container".into(),
        ));
    }
    if let Some(segment) = path.iter().find(|segment| {
        segment.is_empty() || !segment.chars().all(|ch| ch.is_alphanumeric() || ch == '_')
    }) {
        return Err(QueryError::Compile(format!(
            "invalid JSON property path segment '{segment}'; path segments must be identifiers"
        )));
    }
    Ok(format!("$.{}", path.join(".")))
}

fn json_path_expression(column: &str, path: &[String]) -> Result<String, QueryError> {
    let path = validated_json_path(path)?;
    Ok(format!("json_extract({column}, '{path}')"))
}

fn bound_json_path_expression(
    column: &str,
    path: &[String],
    params: &mut Vec<QueryValue>,
) -> Result<String, QueryError> {
    params.push(QueryValue::Text(validated_json_path(path)?));
    Ok(format!("json_extract({column}, ?{})", params.len()))
}

fn order_property_expression(
    property: &PropertyRef,
    kind: &VarKind,
    alias: &str,
    params: &mut Vec<QueryValue>,
) -> Result<String, QueryError> {
    match property {
        PropertyRef::Field(field) => {
            if matches!(field.as_str(), "namespace" | "properties" | "metadata") {
                return Err(QueryError::Compile(
                    "ORDER BY requires a scalar field or explicit JSON path, not namespace or a JSON container".into(),
                ));
            }
            scalar_property_expression(field, kind, alias)
        }
        PropertyRef::JsonPath(path) => {
            let (columns, kind_name) = property_columns(kind);
            if !columns.contains(&"properties") {
                return Err(QueryError::Compile(format!(
                    "{kind_name} variables do not expose JSON properties in ORDER BY"
                )));
            }
            bound_json_path_expression(&format!("{alias}.properties"), path, params)
        }
        PropertyRef::MetadataPath(_) => where_property_expression(property, kind, alias, params),
    }
}

fn order_direction(direction: OrderDirection) -> &'static str {
    match direction {
        OrderDirection::Asc => "ASC",
        OrderDirection::Desc => "DESC",
    }
}

// Inline tests exercise private compiler phases without widening the public API.
#[cfg(test)]
#[path = "sql_tests.rs"]
mod tests;
