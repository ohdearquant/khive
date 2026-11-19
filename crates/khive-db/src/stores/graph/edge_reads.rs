use chrono::{DateTime, TimeZone, Utc};
use khive_storage::types::{
    Edge, EdgeEndpointBaseCounts, EdgeFilter, EdgeSortField, NeighborCursor, NeighborQuery,
    SortDirection, SortOrder,
};
use khive_types::EdgeRelation;
use uuid::Uuid;

pub(super) fn read_edge(row: &rusqlite::Row<'_>) -> Result<Edge, rusqlite::Error> {
    let namespace: String = row.get(0)?;
    let id_str: String = row.get(1)?;
    let source_str: String = row.get(2)?;
    let target_str: String = row.get(3)?;
    let relation_str: String = row.get(4)?;
    let weight: f64 = row.get(5)?;
    let created_micros: i64 = row.get(6)?;
    let updated_micros: i64 = row.get(7)?;
    let deleted_micros: Option<i64> = row.get(8)?;
    let metadata_str: Option<String> = row.get(9)?;
    let target_backend: Option<String> = row.get(10)?;

    let id = parse_uuid(&id_str)?;
    let source_id = parse_uuid(&source_str)?;
    let target_id = parse_uuid(&target_str)?;
    let created_at = micros_to_datetime(created_micros);
    let relation = relation_str.parse::<EdgeRelation>().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let metadata = match metadata_str {
        Some(s) => {
            let v = serde_json::from_str(&s).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    9,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            Some(v)
        }
        None => None,
    };

    Ok(Edge {
        id: id.into(),
        namespace,
        source_id,
        target_id,
        relation,
        weight,
        created_at,
        updated_at: micros_to_datetime(updated_micros),
        deleted_at: deleted_micros.map(micros_to_datetime),
        metadata,
        target_backend,
    })
}

pub(super) fn parse_uuid(s: &str) -> Result<Uuid, rusqlite::Error> {
    Uuid::parse_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

/// Build the `relation IN (...)` / `weight >= ?` `WHERE`-extra clause and the
/// `LIMIT` clause shared by `neighbors` and `neighbors_both_directions` —
/// both filter and cap identically, differing only in which direction(s) the
/// base `SELECT`s cover. `start_param_idx` is the next free `?N` placeholder
/// (both callers bind `namespace` and `node_id` as `?1`/`?2` first).
pub(super) fn neighbor_extra_clause(
    query: &NeighborQuery,
    start_param_idx: usize,
    after: Option<&NeighborCursor>,
    neighbor_kinds: Option<&[String]>,
) -> (String, String, Vec<Box<dyn rusqlite::types::ToSql>>) {
    let mut conditions: Vec<String> = Vec::new();
    let mut extra_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut param_idx = start_param_idx;

    if let Some(ref rels) = query.relations {
        if !rels.is_empty() {
            let placeholders: Vec<String> = rels
                .iter()
                .map(|r| {
                    extra_params.push(Box::new(r.to_string()));
                    let p = format!("?{}", param_idx);
                    param_idx += 1;
                    p
                })
                .collect();
            conditions.push(format!("relation IN ({})", placeholders.join(",")));
        }
    }

    if let Some(min_w) = query.min_weight {
        extra_params.push(Box::new(min_w));
        conditions.push(format!("weight >= ?{}", param_idx));
        param_idx += 1;
    }

    if let Some(cursor) = after {
        extra_params.push(Box::new(cursor.weight));
        let weight_idx = param_idx;
        param_idx += 1;
        extra_params.push(Box::new(cursor.node_id.to_string()));
        let node_idx = param_idx;
        param_idx += 1;
        extra_params.push(Box::new(cursor.edge_id.to_string()));
        let edge_idx = param_idx;
        param_idx += 1;
        conditions.push(format!(
            "(weight < ?{weight_idx} OR (weight = ?{weight_idx} AND node_id > ?{node_idx}) OR (weight = ?{weight_idx} AND node_id = ?{node_idx} AND edge_id > ?{edge_idx}))"
        ));
    }

    if let Some(kinds) = neighbor_kinds.filter(|kinds| !kinds.is_empty()) {
        let placeholders: Vec<String> = kinds
            .iter()
            .map(|kind| {
                extra_params.push(Box::new(kind.clone()));
                let p = format!("?{param_idx}");
                param_idx += 1;
                p
            })
            .collect();
        let entity_placeholders = placeholders.join(",");
        let note_placeholders: Vec<String> = kinds
            .iter()
            .map(|kind| {
                extra_params.push(Box::new(kind.clone()));
                let p = format!("?{param_idx}");
                param_idx += 1;
                p
            })
            .collect();
        conditions.push(format!(
            "(EXISTS (SELECT 1 FROM entities AS neighbor_entities WHERE neighbor_entities.id = node_id AND neighbor_entities.namespace = ?1 AND neighbor_entities.deleted_at IS NULL AND neighbor_entities.kind IN ({entity_placeholders})) OR EXISTS (SELECT 1 FROM notes AS neighbor_notes WHERE neighbor_notes.id = node_id AND neighbor_notes.namespace = ?1 AND neighbor_notes.deleted_at IS NULL AND neighbor_notes.kind IN ({})))",
            note_placeholders.join(",")
        ));
    }

    let where_extra = if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    };

    let limit_clause = if let Some(lim) = query.limit {
        extra_params.push(Box::new(lim as i64));
        format!(" LIMIT ?{}", param_idx)
    } else {
        String::new()
    };

    (where_extra, limit_clause, extra_params)
}

// Test-only counter of storage-level neighbor SELECT executions (`neighbors`
// and `neighbors_both_directions` each issue exactly one `graph_edges`
// query per call). Lets tests assert the query-count halving a
// `Direction::Both` caller gets from `neighbors_both_directions` vs the old
// pattern of two separate `neighbors` calls (ADR-089 context-verb
// optimization). Gated out of release builds — no counter overhead on the
// hot path in production.
#[cfg(test)]
pub(super) static NEIGHBOR_SELECT_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(super) fn count_neighbor_select() {
    NEIGHBOR_SELECT_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(not(test))]
pub(super) fn count_neighbor_select() {}

#[cfg(test)]
pub(crate) fn reset_neighbor_select_count() {
    NEIGHBOR_SELECT_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn neighbor_select_count() -> usize {
    NEIGHBOR_SELECT_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

pub(super) fn micros_to_datetime(micros: i64) -> DateTime<Utc> {
    Utc.timestamp_micros(micros)
        .single()
        .unwrap_or_else(Utc::now)
}

/// Deterministic ORDER BY for edge queries. #1671: an `id` tiebreak is
/// always appended (following the last sort field's direction, DESC for the
/// empty default) so pages over equal sort values keep a total order. That
/// removes tie-order instability only — offset paging can still duplicate or
/// skip rows under concurrent inserts/deletes or sort-key updates (that
/// would need snapshot isolation or keyset pagination).
pub(super) fn edge_order_clause(sort: &[SortOrder<EdgeSortField>]) -> String {
    if sort.is_empty() {
        return " ORDER BY created_at DESC, id DESC".to_string();
    }
    let mut parts: Vec<String> = sort
        .iter()
        .map(|s| {
            let dir = match s.direction {
                SortDirection::Asc => "ASC",
                SortDirection::Desc => "DESC",
            };
            format!("{} {}", edge_sort_col(&s.field), dir)
        })
        .collect();
    let dir = match sort.last().map(|s| &s.direction) {
        Some(SortDirection::Asc) => "ASC",
        _ => "DESC",
    };
    parts.push(format!("id {dir}"));
    format!(" ORDER BY {}", parts.join(", "))
}

/// `CASE` expression classifying one edge endpoint by the base it resolves
/// against. Soft-deleted endpoints are already excluded by
/// [`LIVE_ENDPOINTS_CONDITION`] in the surrounding `WHERE`, so a row reaching
/// this expression has live endpoints or none at all.
///
/// The probe reads the same local tables as [`LIVE_ENDPOINTS_CONDITION`] and
/// [`crate::stores::graph::edge_statements::endpoint_exists_clause`], which is
/// what makes the three agree on what an
/// endpoint is. Two consequences are deliberate. An `annotates` endpoint that
/// is an event or another edge is a live endpoint that is not in either base,
/// so it classifies as `none` and lands in `unresolved`; that is the bucket's
/// meaning, not a miscount, and `annotates` is excluded from structural
/// density anyway. And when ADR-009 edge routing is wired, an endpoint held in
/// another backend will not be in these tables either: `target_backend` is
/// written by nothing today, so no such row exists yet, but whoever wires the
/// routing has to decide what the breakdown should say about it and change
/// this expression deliberately rather than discover it as a silent
/// `unresolved`.
pub(super) fn endpoint_base_case(column: &str) -> String {
    format!(
        "CASE WHEN EXISTS (SELECT 1 FROM entities be WHERE be.id = graph_edges.{column}) \
         THEN 'entity' \
         WHEN EXISTS (SELECT 1 FROM notes bn WHERE bn.id = graph_edges.{column}) \
         THEN 'note' ELSE 'none' END"
    )
}

/// Folds one `(source_base, target_base, count)` group into the tally. An
/// unrecognized pair lands in `unresolved` rather than being dropped, so the
/// buckets keep summing to the live edge total.
pub(super) fn fold_endpoint_base_row(
    counts: &mut EdgeEndpointBaseCounts,
    source: &str,
    target: &str,
    n: u64,
) {
    let slot = match (source, target) {
        ("entity", "entity") => &mut counts.entity_entity,
        ("entity", "note") => &mut counts.entity_note,
        ("note", "entity") => &mut counts.note_entity,
        ("note", "note") => &mut counts.note_note,
        _ => &mut counts.unresolved,
    };
    *slot = slot.saturating_add(n);
}

/// Restricts an edge count to edges whose endpoints are still live.
///
/// A soft delete leaves incident edges in place on purpose (`KhiveRuntime::delete_entity`
/// documents it; only a hard delete purges them). Every reader that walks the graph
/// hydrates the endpoint records, so it cannot reach an edge whose endpoint is
/// tombstoned. A count that includes those edges therefore reports a density no reader
/// can walk, under a `count_scope` that says `live_only`.
///
/// The predicate excludes an endpoint only when it is PRESENT AND tombstoned. An id
/// found in neither table belongs to a substrate this database does not hold, and
/// treating its absence as a tombstone would silently under-count; erring toward
/// counting keeps the failure in the direction the old behaviour already had.
pub(super) const LIVE_ENDPOINTS_CONDITION: &str = "NOT EXISTS (SELECT 1 FROM entities le \
     WHERE le.id = graph_edges.source_id AND le.deleted_at IS NOT NULL) \
     AND NOT EXISTS (SELECT 1 FROM entities le \
     WHERE le.id = graph_edges.target_id AND le.deleted_at IS NOT NULL) \
     AND NOT EXISTS (SELECT 1 FROM notes ln \
     WHERE ln.id = graph_edges.source_id AND ln.deleted_at IS NOT NULL) \
     AND NOT EXISTS (SELECT 1 FROM notes ln \
     WHERE ln.id = graph_edges.target_id AND ln.deleted_at IS NOT NULL)";

/// Append [`LIVE_ENDPOINTS_CONDITION`] to a `WHERE` clause produced by the edge filter
/// builders, which always emit a non-empty ` WHERE ...`.
pub(super) fn with_live_endpoints(where_clause: &str) -> String {
    format!("{where_clause} AND {LIVE_ENDPOINTS_CONDITION}")
}

pub(super) fn build_edge_filter_sql(
    namespace: &str,
    filter: &EdgeFilter,
) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>) {
    build_edge_filter_sql_for_namespaces(&[namespace.to_string()], filter)
}

pub(super) fn build_edge_filter_sql_for_namespaces(
    namespaces: &[String],
    filter: &EdgeFilter,
) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>) {
    let params: Vec<Box<dyn rusqlite::types::ToSql>> = namespaces
        .iter()
        .map(|namespace| -> Box<dyn rusqlite::types::ToSql> { Box::new(namespace.clone()) })
        .collect();
    let namespace_condition = match namespaces.len() {
        0 => "0".to_string(),
        1 => "namespace = ?1".to_string(),
        _ => {
            let placeholders: Vec<String> =
                (1..=namespaces.len()).map(|i| format!("?{i}")).collect();
            format!("namespace IN ({})", placeholders.join(", "))
        }
    };
    build_edge_filter_conditions(namespace_condition, params, filter)
}

/// Same filter conditions as [`build_edge_filter_sql_for_namespaces`], but
/// the namespace set is bound as a single pre-serialized JSON array
/// parameter (`?1`) matched via `json_each` rather than one `?N` per
/// namespace.
///
/// `namespace IN (?1, ?2, ..., ?N)` costs one bound SQLite variable per
/// namespace; a caller with a large visible-namespace set (hundreds to
/// thousands, e.g. a broad `[actor]` visibility grant) can exceed
/// `SQLITE_LIMIT_VARIABLE_NUMBER` (999 by default) before any of the
/// filter's own parameters are even added. Binding one JSON string instead
/// keeps this at O(1) parameters regardless of namespace count, so paged
/// enumeration (which needs one exact-order SQL statement per page — see
/// [`khive_storage::GraphStore::query_edges_in_namespaces`]) never has to chunk the
/// namespace set and cannot silently corrupt paging by doing so (#2088).
pub(super) fn build_edge_filter_sql_for_namespaces_json(
    namespaces_json: &str,
    filter: &EdgeFilter,
) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>) {
    let params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(namespaces_json.to_string())];
    let namespace_condition = "namespace IN (SELECT value FROM json_each(?1))".to_string();
    build_edge_filter_conditions(namespace_condition, params, filter)
}

pub(super) fn build_edge_filter_conditions(
    namespace_condition: String,
    mut params: Vec<Box<dyn rusqlite::types::ToSql>>,
    filter: &EdgeFilter,
) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>) {
    let mut conditions = vec![namespace_condition, "deleted_at IS NULL".to_string()];

    if !filter.ids.is_empty() {
        let placeholders: Vec<String> = filter
            .ids
            .iter()
            .map(|id| {
                params.push(Box::new(id.to_string()));
                format!("?{}", params.len())
            })
            .collect();
        conditions.push(format!("id IN ({})", placeholders.join(",")));
    }

    if !filter.source_ids.is_empty() {
        let placeholders: Vec<String> = filter
            .source_ids
            .iter()
            .map(|id| {
                params.push(Box::new(id.to_string()));
                format!("?{}", params.len())
            })
            .collect();
        conditions.push(format!("source_id IN ({})", placeholders.join(",")));
    }

    if !filter.target_ids.is_empty() {
        let placeholders: Vec<String> = filter
            .target_ids
            .iter()
            .map(|id| {
                params.push(Box::new(id.to_string()));
                format!("?{}", params.len())
            })
            .collect();
        conditions.push(format!("target_id IN ({})", placeholders.join(",")));
    }

    if !filter.relations.is_empty() {
        let placeholders: Vec<String> = filter
            .relations
            .iter()
            .map(|r| {
                params.push(Box::new(r.to_string()));
                format!("?{}", params.len())
            })
            .collect();
        conditions.push(format!("relation IN ({})", placeholders.join(",")));
    }

    if let Some(min_w) = filter.min_weight {
        params.push(Box::new(min_w));
        conditions.push(format!("weight >= ?{}", params.len()));
    }

    if let Some(max_w) = filter.max_weight {
        params.push(Box::new(max_w));
        conditions.push(format!("weight <= ?{}", params.len()));
    }

    if let Some(ref time_range) = filter.created_at {
        if let Some(start) = time_range.start {
            params.push(Box::new(start.timestamp_micros()));
            conditions.push(format!("created_at >= ?{}", params.len()));
        }
        if let Some(end) = time_range.end {
            params.push(Box::new(end.timestamp_micros()));
            conditions.push(format!("created_at < ?{}", params.len()));
        }
    }

    let clause = format!(" WHERE {}", conditions.join(" AND "));
    (clause, params)
}

pub(super) fn edge_sort_col(field: &EdgeSortField) -> &'static str {
    match field {
        EdgeSortField::CreatedAt => "created_at",
        EdgeSortField::Weight => "weight",
        EdgeSortField::Relation => "relation",
    }
}
