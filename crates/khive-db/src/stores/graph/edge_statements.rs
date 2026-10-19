use chrono::{DateTime, Utc};
use khive_storage::types::{Edge, SqlStatement, SqlValue};
use khive_types::EdgeRelation;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Pure statement builders (ADR-099 B3 r6 structural cut) — see
// `crate::stores::entity` for the full rationale. `upsert_edge`/`delete_edge` in `store_impl.rs` and
// `purge_incident_edges` (plus ADR-099's atomic prepare path in
// `khive-runtime`, and `khive-runtime::operations::update_edge`'s
// non-symmetric branch) all call these.
// ---------------------------------------------------------------------------

/// Canonical endpoint order for symmetric relations (F012).
///
/// For `competes_with` and `composed_with`, ensures `source_uuid < target_uuid`
/// so A→B and B→A collapse to a single canonical row in storage.
pub(super) fn canonical_edge_endpoints(
    relation: EdgeRelation,
    source_id: Uuid,
    target_id: Uuid,
) -> (Uuid, Uuid) {
    relation.canonical_endpoints(source_id, target_id)
}

/// Replacement clause shared by canonical and guarded upserts. Live rows use
/// replace semantics. Tombstones are excluded unless resurrection was
/// explicitly requested; only that branch clears `deleted_at`.
pub(super) fn edge_conflict_clause(resurrect: bool) -> String {
    let deleted_at = if resurrect {
        "NULL"
    } else {
        "graph_edges.deleted_at"
    };
    let predicate = if resurrect {
        ""
    } else {
        " WHERE graph_edges.deleted_at IS NULL"
    };
    format!(
        "weight = excluded.weight, \
         updated_at = excluded.updated_at, \
         deleted_at = {deleted_at}, \
         metadata = excluded.metadata, \
         target_backend = excluded.target_backend{predicate}"
    )
}

/// A `WHERE`-clause fragment asserting the id bound to `id_param` (an SQL
/// placeholder like `?3`) resolves to a live edge endpoint — an
/// undeleted entity or note, an event (append-only, no `deleted_at`), or an
/// undeleted edge (the `annotates` relation's target may be any substrate,
/// including another edge; ADR-002/ADR-055). Shared by
/// [`edge_insert_guarded_by_endpoints_statement`] and
/// [`super::edge_upserts::edge_endpoints_exist`] (#769) so the two "does this endpoint still
/// exist" probes — the guarded single-row insert and the guarded batch
/// pre-check — can never drift on which substrates count as valid
/// endpoints.
pub(super) fn endpoint_exists_clause(id_param: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM entities WHERE id = {id_param} AND deleted_at IS NULL) \
         OR EXISTS (SELECT 1 FROM notes WHERE id = {id_param} AND deleted_at IS NULL) \
         OR EXISTS (SELECT 1 FROM events WHERE id = {id_param}) \
         OR EXISTS (SELECT 1 FROM graph_edges WHERE id = {id_param} AND deleted_at IS NULL)"
    )
}

/// Assert an exact live edge snapshot without updating its row or firing write
/// triggers. Retained links additionally require their endpoints to remain live;
/// deletion assertions may remove an annotation whose former target is gone.
pub fn edge_snapshot_assertion_statement(edge: &Edge, require_endpoints: bool) -> SqlStatement {
    let mut sql = "SELECT 1 FROM graph_edges WHERE id=?1 AND namespace=?2 \
        AND source_id=?3 AND target_id=?4 AND relation=?5 \
        AND updated_at=?6 AND deleted_at IS NULL"
        .to_string();
    if require_endpoints {
        sql.push_str(&format!(
            " AND ({}) AND ({})",
            endpoint_exists_clause("?3"),
            endpoint_exists_clause("?4")
        ));
    }
    SqlStatement {
        sql,
        params: vec![
            SqlValue::Text(Uuid::from(edge.id).to_string()),
            SqlValue::Text(edge.namespace.clone()),
            SqlValue::Text(edge.source_id.to_string()),
            SqlValue::Text(edge.target_id.to_string()),
            SqlValue::Text(edge.relation.to_string()),
            SqlValue::Integer(edge.updated_at.timestamp_micros()),
        ],
        label: Some("edge-snapshot-assertion".into()),
    }
}

/// The exact natural-key-upserting `INSERT ... ON CONFLICT` this store's
/// `upsert_edge` issues. Canonicalizes symmetric-relation endpoints first,
/// matching `upsert_edge`'s own call to `canonical_edge_endpoints`.
pub fn edge_upsert_statement(edge: &Edge) -> SqlStatement {
    edge_upsert_statement_with_resurrection(edge, false)
}

/// Policy-aware form of [`edge_upsert_statement`].
pub fn edge_upsert_statement_with_resurrection(edge: &Edge, resurrect: bool) -> SqlStatement {
    let (source_id, target_id) =
        canonical_edge_endpoints(edge.relation, edge.source_id, edge.target_id);
    let metadata_str = edge
        .metadata
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());
    let conflict_clause = edge_conflict_clause(resurrect);
    SqlStatement {
        sql: format!(
            "INSERT INTO graph_edges \
              (namespace, id, source_id, target_id, relation, weight, \
               created_at, updated_at, deleted_at, metadata, target_backend) \
              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
              ON CONFLICT(namespace, id) DO UPDATE SET \
                  source_id = excluded.source_id, \
                  target_id = excluded.target_id, \
                  relation = excluded.relation, \
                  {conflict_clause} \
              ON CONFLICT(namespace, source_id, target_id, relation) DO UPDATE SET \
                  {conflict_clause}"
        ),
        params: vec![
            SqlValue::Text(edge.namespace.clone()),
            SqlValue::Text(Uuid::from(edge.id).to_string()),
            SqlValue::Text(source_id.to_string()),
            SqlValue::Text(target_id.to_string()),
            SqlValue::Text(edge.relation.to_string()),
            SqlValue::Float(edge.weight),
            SqlValue::Integer(edge.created_at.timestamp_micros()),
            SqlValue::Integer(edge.updated_at.timestamp_micros()),
            match edge.deleted_at {
                Some(t) => SqlValue::Integer(t.timestamp_micros()),
                None => SqlValue::Null,
            },
            match metadata_str {
                Some(m) => SqlValue::Text(m),
                None => SqlValue::Null,
            },
            match &edge.target_backend {
                Some(b) => SqlValue::Text(b.clone()),
                None => SqlValue::Null,
            },
        ],
        label: Some("edge-upsert".to_string()),
    }
}

/// Insert a new edge only while both endpoints still exist.
/// Competing IDs and natural keys, including tombstones, cause a constraint
/// error rather than replacing or restoring the competing row. Callers must
/// require one affected row to reject an endpoint removed after prepare.
pub fn edge_insert_only_guarded_by_endpoints_statement(edge: &Edge) -> SqlStatement {
    let mut statement = edge_upsert_statement(edge);
    let src_exists = endpoint_exists_clause("?3");
    let tgt_exists = endpoint_exists_clause("?4");
    statement.sql = format!(
        "INSERT INTO graph_edges \
          (namespace, id, source_id, target_id, relation, weight, \
           created_at, updated_at, deleted_at, metadata, target_backend) \
          SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11 \
          WHERE ({src_exists}) AND ({tgt_exists})"
    );
    statement.label = Some("edge-insert-only-where-endpoints-exist".to_string());
    statement
}

/// Conditional-insert companion to [`edge_upsert_statement`]. Conflicts on
/// either the id or natural key leave the existing edge untouched so callers
/// can read the winner and explicitly reapply their intended delta.
pub fn edge_insert_if_absent_statement(edge: &Edge) -> SqlStatement {
    let mut statement = edge_upsert_statement(edge);
    statement.sql = "INSERT INTO graph_edges \
              (namespace, id, source_id, target_id, relation, weight, \
               created_at, updated_at, deleted_at, metadata, target_backend) \
              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
              ON CONFLICT DO NOTHING"
        .to_string();
    statement.label = Some("edge-insert-if-absent".to_string());
    statement
}

/// Full-edge compare-and-swap update used after caller-side normalization
/// was derived from a read snapshot. Unlike [`edge_upsert_statement`], this
/// never inserts and cannot overwrite a row whose revision or deletion
/// marker moved after the snapshot was read. The replacement revision must
/// also be strictly greater than the persisted snapshot revision; equality
/// is a refused CAS, never a successful write with an unchanged concurrency
/// token. Mirrors `note_replace_if_unchanged_statement`
/// (`crates/khive-db/src/stores/note.rs`). `created_at` is deliberately
/// excluded from the `SET` list, matching the note/entity siblings — a
/// replacement never rewrites the row's original creation time.
pub fn edge_replace_if_unchanged_statement(
    edge: &Edge,
    expected_updated_at: DateTime<Utc>,
    expected_deleted_at: Option<DateTime<Utc>>,
) -> SqlStatement {
    let (source_id, target_id) =
        canonical_edge_endpoints(edge.relation, edge.source_id, edge.target_id);
    let metadata_str = edge
        .metadata
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());
    SqlStatement {
        sql: "UPDATE graph_edges SET \
                namespace = ?1, source_id = ?2, target_id = ?3, relation = ?4, weight = ?5, \
                updated_at = ?6, deleted_at = ?7, metadata = ?8, target_backend = ?9 \
              WHERE id = ?10 AND updated_at = ?11 AND deleted_at IS ?12 \
                AND ?6 > updated_at"
            .to_string(),
        params: vec![
            SqlValue::Text(edge.namespace.clone()),
            SqlValue::Text(source_id.to_string()),
            SqlValue::Text(target_id.to_string()),
            SqlValue::Text(edge.relation.to_string()),
            SqlValue::Float(edge.weight),
            SqlValue::Integer(edge.updated_at.timestamp_micros()),
            match edge.deleted_at {
                Some(t) => SqlValue::Integer(t.timestamp_micros()),
                None => SqlValue::Null,
            },
            match metadata_str {
                Some(m) => SqlValue::Text(m),
                None => SqlValue::Null,
            },
            match &edge.target_backend {
                Some(b) => SqlValue::Text(b.clone()),
                None => SqlValue::Null,
            },
            SqlValue::Text(Uuid::from(edge.id).to_string()),
            SqlValue::Integer(expected_updated_at.timestamp_micros()),
            match expected_deleted_at {
                Some(value) => SqlValue::Integer(value.timestamp_micros()),
                None => SqlValue::Null,
            },
        ],
        label: Some("edge-replace-if-unchanged".to_string()),
    }
}

/// The atomic `link` op's variant of [`edge_upsert_statement`] (ADR-099
/// §B3). Shares the SAME
/// `EDGE_NATURAL_KEY_CONFLICT_SET` conflict-arm text — the two builders
/// cannot diverge on write behavior — but wraps the `INSERT` in a guarded
/// `SELECT ... WHERE EXISTS(...)` that re-probes both endpoints for
/// existence INSIDE the transaction, at commit time, rather than trusting
/// prepare-time validation alone.
///
/// This guard is atomic-`link`-specific, not a `edge_upsert_statement`
/// concern: `LinkPlan`'s own doc comment (`khive-runtime::atomic_plan`)
/// records why it must be commit-time, not prepare-time — a `link` op's
/// async prepare pass (`validate_edge_relation_endpoints`) can run and pass
/// BEFORE an earlier op in the SAME atomic unit (e.g. `delete(X, hard)`)
/// removes that very endpoint; only a commit-time, in-transaction guard
/// closes that intra-batch ordering hazard (ADR-099 acceptance criteria:
/// `[delete(X, hard), link(A, X)]` must fail, not silently create a
/// dangling edge). Canonical `link` has no equivalent need — it executes
/// and commits standalone, with no other op's write interleaved between its
/// own validation and its own write.
#[allow(clippy::too_many_arguments)]
pub fn edge_insert_guarded_by_endpoints_statement(
    namespace: &str,
    edge_id: Uuid,
    source_id: Uuid,
    target_id: Uuid,
    relation: EdgeRelation,
    weight: f64,
    now: i64,
    metadata: Option<&str>,
) -> SqlStatement {
    edge_insert_guarded_by_endpoints_with_resurrection_statement(
        namespace, source_id, target_id, edge_id, relation, weight, now, metadata, false,
    )
}

/// Policy-aware form of [`edge_insert_guarded_by_endpoints_statement`].
#[allow(clippy::too_many_arguments)]
pub fn edge_insert_guarded_by_endpoints_with_resurrection_statement(
    namespace: &str,
    source_id: Uuid,
    target_id: Uuid,
    edge_id: Uuid,
    relation: EdgeRelation,
    weight: f64,
    now: i64,
    metadata: Option<&str>,
    resurrect: bool,
) -> SqlStatement {
    let src_exists = endpoint_exists_clause("?3");
    let tgt_exists = endpoint_exists_clause("?4");
    let conflict_clause = edge_conflict_clause(resurrect);
    SqlStatement {
        sql: format!(
            "INSERT INTO graph_edges \
              (namespace, id, source_id, target_id, relation, weight, \
               created_at, updated_at, metadata) \
              SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8 \
              WHERE ({src_exists}) AND ({tgt_exists}) \
              ON CONFLICT(namespace, source_id, target_id, relation) DO UPDATE SET \
                  {conflict_clause}"
        ),
        params: vec![
            SqlValue::Text(namespace.to_string()),
            SqlValue::Text(edge_id.to_string()),
            SqlValue::Text(source_id.to_string()),
            SqlValue::Text(target_id.to_string()),
            SqlValue::Text(relation.as_str().to_string()),
            SqlValue::Float(weight),
            SqlValue::Integer(now),
            match metadata {
                Some(m) => SqlValue::Text(m.to_string()),
                None => SqlValue::Null,
            },
        ],
        label: Some("atomic-link-insert-edge-where-exists".to_string()),
    }
}

/// Atomic `link` create arm. Unlike the compatibility upsert builder above,
/// this statement never changes a row that appeared after prepare: a natural-
/// key conflict affects zero rows and therefore fails the caller's guard.
#[allow(clippy::too_many_arguments)]
pub fn edge_insert_new_guarded_by_endpoints_statement(
    namespace: &str,
    edge_id: Uuid,
    source_id: Uuid,
    target_id: Uuid,
    relation: EdgeRelation,
    weight: f64,
    now: i64,
    metadata: Option<&str>,
) -> SqlStatement {
    let src_exists = endpoint_exists_clause("?3");
    let tgt_exists = endpoint_exists_clause("?4");
    SqlStatement {
        sql: format!(
            "INSERT INTO graph_edges \
              (namespace, id, source_id, target_id, relation, weight, \
               created_at, updated_at, metadata) \
              SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8 \
              WHERE ({src_exists}) AND ({tgt_exists}) \
              ON CONFLICT DO NOTHING"
        ),
        params: vec![
            SqlValue::Text(namespace.to_string()),
            SqlValue::Text(edge_id.to_string()),
            SqlValue::Text(source_id.to_string()),
            SqlValue::Text(target_id.to_string()),
            SqlValue::Text(relation.to_string()),
            SqlValue::Float(weight),
            SqlValue::Integer(now),
            metadata
                .map(|value| SqlValue::Text(value.to_string()))
                .unwrap_or(SqlValue::Null),
        ],
        label: Some("edge-link-create-if-absent-and-endpoints-exist".to_string()),
    }
}

/// Atomic `link` replace/resurrection arm. The natural-key row and both
/// concurrency markers must still match the prepare snapshot, and both
/// endpoints must still exist. Clearing `deleted_at` is deliberate here:
/// callers only build this statement for a live replacement or an explicitly
/// authorized resurrection.
pub fn edge_link_replace_if_unchanged_and_endpoints_exist_statement(
    previous: &Edge,
    weight: f64,
    now: i64,
    metadata: Option<&str>,
) -> SqlStatement {
    let src_exists = endpoint_exists_clause("?6");
    let tgt_exists = endpoint_exists_clause("?7");
    SqlStatement {
        sql: format!(
            "UPDATE graph_edges SET \
               weight = ?1, updated_at = ?2, deleted_at = NULL, \
               metadata = ?3, target_backend = NULL \
             WHERE namespace = ?4 AND id = ?5 \
               AND source_id = ?6 AND target_id = ?7 AND relation = ?8 \
               AND updated_at = ?9 AND deleted_at IS ?10 AND ?2 > updated_at \
               AND ({src_exists}) AND ({tgt_exists})"
        ),
        params: vec![
            SqlValue::Float(weight),
            SqlValue::Integer(now),
            metadata
                .map(|value| SqlValue::Text(value.to_string()))
                .unwrap_or(SqlValue::Null),
            SqlValue::Text(previous.namespace.clone()),
            SqlValue::Text(Uuid::from(previous.id).to_string()),
            SqlValue::Text(previous.source_id.to_string()),
            SqlValue::Text(previous.target_id.to_string()),
            SqlValue::Text(previous.relation.to_string()),
            SqlValue::Integer(previous.updated_at.timestamp_micros()),
            previous
                .deleted_at
                .map(|value| SqlValue::Integer(value.timestamp_micros()))
                .unwrap_or(SqlValue::Null),
        ],
        label: Some("edge-link-replace-if-unchanged-and-endpoints-exist".to_string()),
    }
}

/// The exact soft-delete `UPDATE` this store's `delete_edge(Soft)` issues.
pub fn edge_soft_delete_statement(id: Uuid, now: i64) -> SqlStatement {
    SqlStatement {
        sql: "UPDATE graph_edges SET deleted_at = ?2, updated_at = ?2 \
              WHERE id = ?1 AND deleted_at IS NULL"
            .to_string(),
        params: vec![SqlValue::Text(id.to_string()), SqlValue::Integer(now)],
        label: Some("edge-delete-soft".to_string()),
    }
}

/// Rewire a merge edge without changing its other fields or adding revision guards.
/// The caller owns conflict checks and the transaction; `namespace` is the edge's
/// stored attribution, which may differ from either merge participant's namespace.
pub fn edge_merge_rewire_statement(
    namespace: &str,
    id: Uuid,
    source_id: Uuid,
    target_id: Uuid,
    updated_at: i64,
) -> SqlStatement {
    SqlStatement::new(
        "UPDATE graph_edges SET \
                     source_id = ?1, target_id = ?2, updated_at = ?3 \
                     WHERE namespace = ?4 AND id = ?5",
        vec![
            SqlValue::Text(source_id.to_string()),
            SqlValue::Text(target_id.to_string()),
            SqlValue::Integer(updated_at),
            SqlValue::Text(namespace.to_string()),
            SqlValue::Text(id.to_string()),
        ],
    )
}

/// The exact hard-delete `DELETE` this store's `delete_edge(Hard)` issues.
pub fn edge_hard_delete_statement(id: Uuid) -> SqlStatement {
    SqlStatement {
        sql: "DELETE FROM graph_edges WHERE id = ?1".to_string(),
        params: vec![SqlValue::Text(id.to_string())],
        label: Some("edge-delete-hard".to_string()),
    }
}

/// A common table expression `edge_ns(namespace)` holding every namespace
/// present in `graph_edges`, read with one index seek per namespace
/// (`min(namespace)` above the previous one), followed by a final NULL row.
///
/// Every `graph_edges` index leads with `namespace`, so a lookup by endpoint
/// alone (`source_id = ? OR target_id = ?`) cannot seek and reads every edge.
/// Joined to this walk (`edge_ns CROSS JOIN graph_edges`, which keeps the walk
/// as the outer loop) the same lookup seeks `(namespace, source_id, ...)` and
/// `(namespace, target_id, ...)` once per namespace, so its cost follows the
/// number of namespaces and incident edges instead of the size of the table.
pub(crate) const EDGE_NAMESPACES_CTE: &str = "WITH RECURSIVE edge_ns(namespace) AS ( \
     SELECT (SELECT min(namespace) FROM graph_edges) \
     UNION ALL \
     SELECT (SELECT min(namespace) FROM graph_edges WHERE namespace > edge_ns.namespace) \
     FROM edge_ns WHERE edge_ns.namespace IS NOT NULL)";

/// The exact cascade `DELETE` this store's `purge_incident_edges` issues.
///
/// Removes every edge whose source or target is `node_id`, in any namespace,
/// found by walking the namespaces present in `graph_edges` and seeking each
/// one. A self-loop matches both arms and is deleted once.
pub fn purge_incident_edges_statement(node_id: Uuid) -> SqlStatement {
    SqlStatement {
        sql: format!(
            "DELETE FROM graph_edges WHERE id IN ({EDGE_NAMESPACES_CTE} \
             SELECT e.id FROM edge_ns CROSS JOIN graph_edges AS e \
               ON e.namespace = edge_ns.namespace AND e.source_id = ?1 \
             UNION ALL \
             SELECT e.id FROM edge_ns CROSS JOIN graph_edges AS e \
               ON e.namespace = edge_ns.namespace AND e.target_id = ?1)"
        ),
        params: vec![SqlValue::Text(node_id.to_string())],
        label: Some("edge-purge-incident".to_string()),
    }
}
