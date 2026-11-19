// Named imports rather than `use super::*;`: the static SQL census refuses an
// `include_str!` loader in a module that has a glob import in scope.
use khive_storage::types::{
    Edge, EdgeUpsertDisposition, EdgeUpsertRefusal, EdgeUpsertRequest, EdgeUpsertResult,
    GuardedEdgeBatchRefusal, GuardedEdgeBatchUpsertOutcome, GuardedEdgeUpsertOutcome,
    MissingEndpoints,
};
use khive_types::EdgeRelation;
use rusqlite::OptionalExtension;
use uuid::Uuid;

use crate::sql_bridge::bind_params;

use super::edge_reads::read_edge;
use super::edge_statements::{
    canonical_edge_endpoints, edge_upsert_statement_with_resurrection, endpoint_exists_clause,
};
#[cfg(test)]
use super::tests;

/// Standalone existence probe for both endpoints of a would-be edge (#769),
/// matching the `WHERE EXISTS(...)` shape
/// [`super::edge_statements::edge_insert_guarded_by_endpoints_statement`] embeds in its own guarded
/// `INSERT`. Returns per-endpoint existence (not a single AND'd bool) so
/// callers can report exactly which side was missing. See
/// `crates/khive-db/docs/api/graph.md` for its two call sites and why both
/// need in-transaction, not reconstructed-after-the-fact, results.
pub(super) fn edge_endpoints_exist(
    conn: &rusqlite::Connection,
    source_id: Uuid,
    target_id: Uuid,
) -> Result<MissingEndpoints, rusqlite::Error> {
    let src_exists = endpoint_exists_clause("?1");
    let tgt_exists = endpoint_exists_clause("?2");
    let sql = format!("SELECT ({src_exists}), ({tgt_exists})");
    conn.query_row(
        &sql,
        rusqlite::params![source_id.to_string(), target_id.to_string()],
        |row| {
            let src_exists: bool = row.get(0)?;
            let tgt_exists: bool = row.get(1)?;
            Ok(MissingEndpoints {
                source: !src_exists,
                target: !tgt_exists,
            })
        },
    )
}

pub(super) fn edge_by_natural_key_including_deleted(
    conn: &rusqlite::Connection,
    edge: &Edge,
) -> Result<Option<Edge>, rusqlite::Error> {
    edge_by_natural_key_parts_including_deleted(
        conn,
        &edge.namespace,
        edge.source_id,
        edge.target_id,
        edge.relation,
    )
}

pub(super) fn edge_by_natural_key_parts_including_deleted(
    conn: &rusqlite::Connection,
    namespace: &str,
    source_id: Uuid,
    target_id: Uuid,
    relation: EdgeRelation,
) -> Result<Option<Edge>, rusqlite::Error> {
    let (source_id, target_id) = canonical_edge_endpoints(relation, source_id, target_id);
    conn.query_row(
        "SELECT namespace, id, source_id, target_id, relation, weight, \
                created_at, updated_at, deleted_at, metadata, target_backend \
         FROM graph_edges \
         WHERE namespace = ?1 AND source_id = ?2 AND target_id = ?3 AND relation = ?4",
        rusqlite::params![
            namespace,
            source_id.to_string(),
            target_id.to_string(),
            relation.as_str(),
        ],
        read_edge,
    )
    .optional()
}

/// Apply one replacement-style upsert and derive its disposition/preimage on
/// the same write connection. Callers keep this inside one write transaction.
pub(super) fn observed_edge_upsert(
    conn: &rusqlite::Connection,
    request: &EdgeUpsertRequest,
    guard_endpoints: bool,
) -> Result<GuardedEdgeUpsertOutcome, rusqlite::Error> {
    let (source_id, target_id) = canonical_edge_endpoints(
        request.edge.relation,
        request.edge.source_id,
        request.edge.target_id,
    );
    if guard_endpoints {
        // Test-only rendezvous proving the endpoint decision is made while
        // this write transaction still excludes racing writers.
        #[cfg(test)]
        tests::insert_probe_seam::hook((source_id, target_id));
        let missing = edge_endpoints_exist(conn, source_id, target_id)?;
        if missing.any() {
            return Ok(GuardedEdgeUpsertOutcome::Refused(
                EdgeUpsertRefusal::MissingEndpoints(missing),
            ));
        }
    }

    let previous = edge_by_natural_key_including_deleted(conn, &request.edge)?;
    if let Some(edge) = previous.as_ref() {
        if edge.deleted_at.is_some() && !request.resurrect {
            return Ok(GuardedEdgeUpsertOutcome::Refused(
                EdgeUpsertRefusal::ResurrectionRequired { edge: edge.clone() },
            ));
        }
    }

    let statement = edge_upsert_statement_with_resurrection(&request.edge, request.resurrect);
    let mut stmt = conn.prepare_cached(&statement.sql)?;
    bind_params(&mut stmt, &statement.params)?;
    let affected = stmt.raw_execute()?;
    if affected == 0 {
        let edge = edge_by_natural_key_including_deleted(conn, &request.edge)?
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        return Ok(GuardedEdgeUpsertOutcome::Refused(
            EdgeUpsertRefusal::ResurrectionRequired { edge },
        ));
    }

    let edge = edge_by_natural_key_including_deleted(conn, &request.edge)?
        .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    let disposition = match previous.as_ref().and_then(|edge| edge.deleted_at) {
        None if previous.is_none() => EdgeUpsertDisposition::Created,
        None => EdgeUpsertDisposition::Updated,
        Some(_) => EdgeUpsertDisposition::Resurrected,
    };
    Ok(GuardedEdgeUpsertOutcome::Written(EdgeUpsertResult {
        edge,
        disposition,
        previous,
    }))
}

/// Shared DML-only batch engine for observed and legacy upserts on both writer
/// routes. The caller owns the transaction and rolls back SQL failures.
pub(super) fn observed_edge_batch_upsert(
    conn: &rusqlite::Connection,
    requests: &[EdgeUpsertRequest],
    guard_endpoints: bool,
) -> Result<GuardedEdgeBatchUpsertOutcome, rusqlite::Error> {
    // Preflight every refusal before the first mutation so the successful
    // return path is genuinely all-or-nothing even under WriterTask, whose
    // transaction commits any `Ok(...)` closure result.
    for (entry_index, request) in requests.iter().enumerate() {
        let (source_id, target_id) = canonical_edge_endpoints(
            request.edge.relation,
            request.edge.source_id,
            request.edge.target_id,
        );
        if guard_endpoints {
            let missing = edge_endpoints_exist(conn, source_id, target_id)?;
            if missing.any() {
                return Ok(GuardedEdgeBatchUpsertOutcome {
                    rows: Vec::new(),
                    refusal: Some(GuardedEdgeBatchRefusal {
                        entry_index,
                        reason: EdgeUpsertRefusal::MissingEndpoints(missing),
                    }),
                });
            }
        }
        if let Some(edge) = edge_by_natural_key_including_deleted(conn, &request.edge)? {
            if edge.deleted_at.is_some() && !request.resurrect {
                return Ok(GuardedEdgeBatchUpsertOutcome {
                    rows: Vec::new(),
                    refusal: Some(GuardedEdgeBatchRefusal {
                        entry_index,
                        reason: EdgeUpsertRefusal::ResurrectionRequired { edge },
                    }),
                });
            }
        }
    }

    let mut rows = Vec::with_capacity(requests.len());
    for request in requests {
        match observed_edge_upsert(conn, request, false)? {
            GuardedEdgeUpsertOutcome::Written(row) => rows.push(row),
            GuardedEdgeUpsertOutcome::Refused(_) => {
                return Err(rusqlite::Error::ExecuteReturnedResults)
            }
        }
    }
    Ok(GuardedEdgeBatchUpsertOutcome {
        rows,
        refusal: None,
    })
}
