use super::{
    base_entity_rule_allows, edge_row_budget_bytes, endpoint_matches, EdgeEndpointRule,
    EdgeRelation, HashMap, HashSet, MergeEdgePreimage, MergeTxBudget, OptionalExtension,
    SqliteError, Uuid, VecDeque,
};

// REASON: EdgeRow fields are populated via rusqlite row mapping. The struct is fully
// constructed even when not all fields are read back after construction. The complete
// field mapping guards against column-order bugs when the schema changes.
#[derive(Clone)]
pub(super) struct EdgeRow {
    pub(super) id: Uuid,
    /// The edge's own attribution namespace (khive#1236) — may differ from the
    /// merge's target namespace, since by-ID edge endpoints are namespace-agnostic
    /// (ADR-007 Rev 6) and an edge is stamped with its *creator's* namespace, not
    /// either endpoint's. All row-scoped SQL against this edge (conflict probe,
    /// update, delete) must key off this field, never the merge's `namespace` arg.
    pub(super) namespace: String,
    pub(super) source_id: Uuid,
    pub(super) target_id: Uuid,
    pub(super) relation: String,
    pub(super) weight: f64,
    pub(super) created_at: i64,
    pub(super) updated_at: i64,
    pub(super) deleted_at: Option<i64>,
    pub(super) target_backend: Option<String>,
    pub(super) metadata: Option<String>,
}

impl EdgeRow {
    /// Decode the shared incident-edge projection after the caller has parsed its ID.
    /// Keeping ID handling separate lets cascade collection skip planned deletions
    /// before decoding the remaining columns.
    pub(super) fn from_row_with_id(row: &rusqlite::Row<'_>, id: Uuid) -> Result<Self, SqliteError> {
        let parse_id =
            |s: String| Uuid::parse_str(&s).map_err(|e| SqliteError::InvalidData(e.to_string()));
        Ok(Self {
            id,
            namespace: row.get(1)?,
            source_id: parse_id(row.get(2)?)?,
            target_id: parse_id(row.get(3)?)?,
            relation: row.get(4)?,
            weight: row.get(5)?,
            created_at: row.get(6)?,
            updated_at: row.get(7)?,
            deleted_at: row.get(8)?,
            target_backend: row.get(9)?,
            metadata: row.get(10)?,
        })
    }
}

pub(super) fn edge_row_preimage(edge: &EdgeRow) -> Result<MergeEdgePreimage, SqliteError> {
    let metadata = edge
        .metadata
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| SqliteError::InvalidData(error.to_string()))?;
    Ok(MergeEdgePreimage {
        id: edge.id,
        namespace: edge.namespace.clone(),
        source_id: edge.source_id,
        target_id: edge.target_id,
        relation: edge.relation.clone(),
        weight: edge.weight,
        created_at: edge.created_at,
        updated_at: edge.updated_at,
        deleted_at: edge.deleted_at,
        target_backend: edge.target_backend.clone(),
        metadata,
    })
}

/// Capture every row that the accepted hard-edge-delete cascade would remove
/// when `root_edge_id` is purged. The traversal is recursive because an
/// `annotates` edge may itself be an annotation target. Rows that also touch a
/// merge participant use their transaction-start snapshot from `original_edges`
/// so the preimage never reflects an earlier rewire in the same merge. Planned
/// deletions are excluded before budget charging so dry runs match committed
/// merges even when two cascades overlap.
pub(super) fn collect_merge_drop_incident_edge_preimages(
    conn: &rusqlite::Connection,
    root_edge_id: Uuid,
    original_edges: &HashMap<Uuid, EdgeRow>,
    planned_deleted_edge_ids: &HashSet<Uuid>,
    budget: &mut MergeTxBudget,
    budget_context: &str,
) -> Result<Vec<MergeEdgePreimage>, SqliteError> {
    let parse_id =
        |s: String| Uuid::parse_str(&s).map_err(|e| SqliteError::InvalidData(e.to_string()));
    let mut queue = VecDeque::from([root_edge_id]);
    let mut seen = HashSet::from([root_edge_id]);
    let mut preimages = Vec::new();

    while let Some(target_edge_id) = queue.pop_front() {
        let mut stmt = conn.prepare(
            "SELECT id, namespace, source_id, target_id, relation, weight, created_at, \
                    updated_at, deleted_at, target_backend, metadata \
             FROM graph_edges WHERE source_id = ?1 OR target_id = ?1 ORDER BY id",
        )?;
        let mut rows = stmt.query(rusqlite::params![target_edge_id.to_string()])?;
        while let Some(row) = rows.next()? {
            let id = parse_id(row.get(0)?)?;
            if planned_deleted_edge_ids.contains(&id) {
                continue;
            }
            let edge = EdgeRow::from_row_with_id(row, id)?;
            budget.charge(1, edge_row_budget_bytes(&edge), budget_context)?;
            if !seen.insert(edge.id) {
                continue;
            }
            let preimage = match original_edges.get(&edge.id) {
                Some(original) => edge_row_preimage(original)?,
                None => edge_row_preimage(&edge)?,
            };
            queue.push_back(edge.id);
            preimages.push(preimage);
        }
    }

    Ok(preimages)
}

pub(super) fn delete_merge_drop_edges(
    conn: &rusqlite::Connection,
    root: &EdgeRow,
    incident_preimages: &[MergeEdgePreimage],
) -> Result<(), SqliteError> {
    for edge in incident_preimages.iter().rev() {
        conn.execute(
            khive_db::stores::graph::EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL,
            rusqlite::params![&edge.namespace, edge.id.to_string()],
        )?;
    }
    conn.execute(
        khive_db::stores::graph::EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL,
        rusqlite::params![&root.namespace, root.id.to_string()],
    )?;
    Ok(())
}

/// Resolves the substrate (`"entity"` or `"note"`), kind, and entity_type (entities
/// only) of an edge endpoint by id, namespace-agnostically — by-ID resolution is
/// namespace-agnostic by design (ADR-007 Rev 6), and an edge's non-merging endpoint
/// may live in any namespace. Returns `None` if `id` resolves to neither table
/// (e.g. a hard-deleted or otherwise absent record); callers must treat that as
/// "the endpoint contract cannot be evaluated" and drop the edge rather than
/// silently allow it through.
/// `(substrate, kind, entity_type)` for a resolved merge-edge endpoint.
type MergeEdgeEndpointInfo = (&'static str, String, Option<String>);

fn resolve_merge_edge_endpoint(
    conn: &rusqlite::Connection,
    id: Uuid,
) -> Result<Option<MergeEdgeEndpointInfo>, SqliteError> {
    let id_str = id.to_string();
    if let Some((kind, entity_type)) = conn
        .query_row(
            "SELECT kind, entity_type FROM entities WHERE id = ?1",
            rusqlite::params![&id_str],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()
        .map_err(SqliteError::Rusqlite)?
    {
        return Ok(Some(("entity", kind, entity_type)));
    }
    if let Some(kind) = conn
        .query_row(
            "SELECT kind FROM notes WHERE id = ?1",
            rusqlite::params![&id_str],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(SqliteError::Rusqlite)?
    {
        return Ok(Some(("note", kind, None)));
    }
    Ok(None)
}

/// [`resolve_merge_edge_endpoint`] with the resolved row charged against the
/// merge transaction budget — endpoint resolution reads one row per non-merging
/// endpoint, so a hub merge's contract checks are part of its materialization.
pub(super) fn resolve_merge_edge_endpoint_budgeted(
    conn: &rusqlite::Connection,
    id: Uuid,
    budget: &mut MergeTxBudget,
) -> Result<Option<MergeEdgeEndpointInfo>, SqliteError> {
    let info = resolve_merge_edge_endpoint(conn, id)?;
    if let Some((_, kind, entity_type)) = &info {
        budget.charge(
            1,
            kind.len() + entity_type.as_deref().map_or(0, str::len),
            "resolving rewire endpoint contracts",
        )?;
    }
    Ok(info)
}

/// `true` if `(src_sub, src_kind, src_type) -[relation]-> (tgt_sub, tgt_kind, tgt_type)`
/// is permitted under the base ADR-002 entity allowlist or a pack-declared
/// `EdgeEndpointRule` — the exact same `endpoint_matches` semantics `link`'s
/// `validate_edge_relation_endpoints` applies (khive-runtime/src/operations.rs),
/// reused here rather than re-derived, per the #543/#621 lesson that a parallel
/// matcher drifts out of sync with the validator.
///
/// `annotates` is exempt: its source-must-be-a-note constraint is enforced at
/// edge creation and unchanged by rewiring (an entity merge only ever rewires
/// its unfiltered target; a note merge rewiring the source substitutes another
/// note), and its target may be any substrate. Callers short-circuit `annotates`
/// before endpoint resolution — an annotates target may be an event or an edge,
/// which `resolve_merge_edge_endpoint` cannot resolve; the exemption here is
/// kept as defense in depth.
// REASON: the two endpoints each need substrate/kind/entity_type independently —
// collapsing them into a tuple/struct would obscure which side is which at call
// sites that already pass them as separate locals.
#[allow(clippy::too_many_arguments)]
pub(super) fn merge_rewire_endpoint_contract_allows(
    pack_rules: &[EdgeEndpointRule],
    relation: EdgeRelation,
    src_sub: &str,
    src_kind: &str,
    src_type: Option<&str>,
    tgt_sub: &str,
    tgt_kind: &str,
    tgt_type: Option<&str>,
) -> bool {
    if relation == EdgeRelation::Annotates {
        return true;
    }
    // Same-substrate relations permit any note→note pair unconditionally,
    // matching `validate_edge_relation_endpoints`'s `(Note, Note) => {}` arm.
    if src_sub == "note" && tgt_sub == "note" && crate::pack::is_special_relation(relation) {
        return true;
    }
    if src_sub == "entity"
        && tgt_sub == "entity"
        && base_entity_rule_allows(src_kind, relation, tgt_kind)
    {
        return true;
    }
    pack_rules.iter().any(|r| {
        r.relation == relation
            && endpoint_matches(&r.source, src_sub, src_kind, src_type)
            && endpoint_matches(&r.target, tgt_sub, tgt_kind, tgt_type)
    })
}
