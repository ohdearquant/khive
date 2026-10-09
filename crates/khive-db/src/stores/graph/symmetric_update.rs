//! Guarded symmetric-edge conflict resolution and its shared statement builders.

use std::sync::Arc;

use khive_storage::graph::{SymmetricEdgeUpdateOutcome, SymmetricEdgeUpdateRequest};
use khive_storage::{SqlStatement, SqlValue, StorageCapability, StorageError, StorageResult};
use khive_types::EdgeRelation;
use rusqlite::OptionalExtension;
use uuid::Uuid;

use super::{map_sqlite_err, SqlGraphStore};
use crate::pool::RuntimeWriteOperation;
use crate::SqliteError;

// Only the conflict probe is shared with atomic prepare. Atomic writes use
// their own self-guarding statements; merge keeps the unguarded delete builder.
// Preserve these public builder paths through the parent module's re-exports.
pub const EDGE_SYMMETRIC_CONFLICT_PROBE_SQL: &str = "SELECT id FROM graph_edges \
     WHERE namespace = ?1 AND source_id = ?2 AND target_id = ?3 \
     AND relation = ?4 AND id != ?5";

pub const EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL: &str =
    "DELETE FROM graph_edges WHERE namespace = ?1 AND id = ?2";

/// Canonical `update_edge`'s guarded variant of
/// [`EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL`]: `?3`/`?4` pin the fetched
/// snapshot's `updated_at`/`deleted_at` so a writer whose edge changed
/// concurrently after it read that snapshot cannot delete the row out from
/// under the concurrent write, even though a canonical survivor genuinely
/// exists at the natural key. Zero affected rows means stale, not
/// "no conflict" — the caller has already confirmed a conflicting canonical
/// row exists before running this statement. Deliberately a DIFFERENT
/// constant from the unguarded one above: merge's predicate-based rewrites
/// (`khive-runtime::curation`) intentionally keep running the unguarded form
/// inside their own single writer transaction and must not be changed to
/// bind this one.
pub const EDGE_SYMMETRIC_DELETE_NONCANONICAL_GUARDED_SQL: &str =
    "DELETE FROM graph_edges WHERE namespace = ?1 AND id = ?2 \
     AND updated_at = ?3 AND deleted_at IS ?4";

/// Case (a) update, guarded on the fetched snapshot's revision and deletion
/// marker: `?9`/`?10` pin `updated_at`/`deleted_at` as read, and `?5 >
/// updated_at` requires the replacement revision to strictly advance —
/// mirroring `edge_replace_if_unchanged_statement`'s guard so the symmetric
/// path cannot silently overwrite a concurrent writer's change between the
/// snapshot read and this write.
pub const EDGE_SYMMETRIC_UPDATE_INPLACE_SQL: &str = "UPDATE graph_edges SET \
     source_id = ?1, target_id = ?2, relation = ?3, \
     weight = ?4, updated_at = ?5, metadata = ?6 \
     WHERE namespace = ?7 AND id = ?8 \
       AND updated_at = ?9 AND deleted_at IS ?10 \
       AND ?5 > updated_at";

/// Plan-shape builder for [`EDGE_SYMMETRIC_CONFLICT_PROBE_SQL`] — the
/// async prepare-time conflict probe.
pub fn edge_symmetric_conflict_probe_statement(
    namespace: &str,
    canon_src: Uuid,
    canon_tgt: Uuid,
    relation: EdgeRelation,
    exclude_id: Uuid,
) -> SqlStatement {
    SqlStatement {
        sql: EDGE_SYMMETRIC_CONFLICT_PROBE_SQL.to_string(),
        params: vec![
            SqlValue::Text(namespace.to_string()),
            SqlValue::Text(canon_src.to_string()),
            SqlValue::Text(canon_tgt.to_string()),
            SqlValue::Text(relation.to_string()),
            SqlValue::Text(exclude_id.to_string()),
        ],
        label: Some("edge-symmetric-conflict-probe".to_string()),
    }
}

/// Plan-shape builder for [`EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL`] —
/// case (b): a canonical row already exists, delete the requested row.
pub fn edge_symmetric_delete_noncanonical_statement(namespace: &str, id: Uuid) -> SqlStatement {
    SqlStatement {
        sql: EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL.to_string(),
        params: vec![
            SqlValue::Text(namespace.to_string()),
            SqlValue::Text(id.to_string()),
        ],
        label: Some("edge-symmetric-delete-noncanonical".to_string()),
    }
}

/// Plan-shape builder for [`EDGE_SYMMETRIC_UPDATE_INPLACE_SQL`] —
/// case (a): no conflict, update the requested row in place, guarded on the
/// fetched snapshot's revision and deletion marker.
#[allow(clippy::too_many_arguments)]
pub fn edge_symmetric_update_inplace_statement(
    namespace: &str,
    id: Uuid,
    canon_src: Uuid,
    canon_tgt: Uuid,
    relation: EdgeRelation,
    weight: f64,
    updated_at_micros: i64,
    metadata: Option<&str>,
    expected_updated_at_micros: i64,
    expected_deleted_at_micros: Option<i64>,
) -> SqlStatement {
    SqlStatement {
        sql: EDGE_SYMMETRIC_UPDATE_INPLACE_SQL.to_string(),
        params: vec![
            SqlValue::Text(canon_src.to_string()),
            SqlValue::Text(canon_tgt.to_string()),
            SqlValue::Text(relation.to_string()),
            SqlValue::Float(weight),
            SqlValue::Integer(updated_at_micros),
            match metadata {
                Some(m) => SqlValue::Text(m.to_string()),
                None => SqlValue::Null,
            },
            SqlValue::Text(namespace.to_string()),
            SqlValue::Text(id.to_string()),
            SqlValue::Integer(expected_updated_at_micros),
            match expected_deleted_at_micros {
                Some(value) => SqlValue::Integer(value),
                None => SqlValue::Null,
            },
        ],
        label: Some("edge-symmetric-update-inplace".to_string()),
    }
}

impl SqlGraphStore {
    pub(super) async fn update_symmetric_edge(
        &self,
        request: SymmetricEdgeUpdateRequest,
    ) -> StorageResult<SymmetricEdgeUpdateOutcome> {
        let ns = request.namespace;
        let edge_id_str = request.id.to_string();
        let canon_src_str = request.source_id.to_string();
        let canon_tgt_str = request.target_id.to_string();
        let relation_str = request.relation.to_string();
        let weight = request.weight;
        let metadata = request
            .metadata
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        let expected_updated_at_micros = request.expected_updated_at_micros;
        let expected_deleted_at_micros = request.expected_deleted_at_micros;
        let pool = Arc::clone(&self.pool);
        // This operation keeps its existing runtime admission/telemetry route.
        // The generic graph helper uses bounded queue admission instead.
        let writer_task =
            pool.writer_task_for_runtime_write(RuntimeWriteOperation::UpdateSymmetricEdge)?;
        let dml = move |conn: &rusqlite::Connection| {
            update_edge_symmetric_dml(
                conn,
                &ns,
                &edge_id_str,
                &canon_src_str,
                &canon_tgt_str,
                &relation_str,
                weight,
                metadata,
                expected_updated_at_micros,
                expected_deleted_at_micros,
            )
        };
        if let Some(writer_task) = writer_task {
            writer_task
                .send(move |conn| {
                    dml(conn).map_err(|error| {
                        StorageError::driver(StorageCapability::Graph, "update_edge", error)
                    })
                })
                .await
        } else {
            tokio::task::spawn_blocking(move || {
                let guard = pool.writer()?;
                guard.transaction(dml)
            })
            .await
            .map_err(|error| StorageError::driver(StorageCapability::Graph, "update_edge", error))?
            .map_err(|error| map_sqlite_err(error, "update_edge"))
        }
    }
}

// DML only: the selected writer route owns BEGIN/COMMIT/ROLLBACK. Decode the
// survivor ID only after commit in the runtime, preserving legacy error timing.
#[allow(clippy::too_many_arguments)]
fn update_edge_symmetric_dml(
    conn: &rusqlite::Connection,
    ns: &str,
    edge_id_str: &str,
    canon_src_str: &str,
    canon_tgt_str: &str,
    relation_str: &str,
    weight: f64,
    metadata: Option<String>,
    expected_updated_at_micros: i64,
    expected_deleted_at_micros: Option<i64>,
) -> Result<SymmetricEdgeUpdateOutcome, SqliteError> {
    // `updated_at` is stored in MICROSECONDS on `graph_edges` (every other
    // write path — `edge_upsert_statement`, `edge_soft_delete_statement` —
    // uses `timestamp_micros()`; the column is read back via
    // `micros_to_datetime`). `timestamp()` (seconds) here was a
    // pre-existing bug in this raw-SQL path, found while unifying it with
    // the atomic builder (which already used `timestamp_micros()`
    // correctly).
    //
    // The replacement revision must strictly advance past the snapshot
    // even when two operations land inside one clock microsecond;
    // saturating to i64::MAX would let the CAS accept a write without
    // advancing its revision, so that is not a valid fallback (mirrors
    // the note path).
    let minimum_updated_at_micros = expected_updated_at_micros.checked_add(1).ok_or_else(|| {
        SqliteError::InvalidData(format!(
            "update_edge: edge {edge_id_str} updated_at is already at i64::MAX \
                     and cannot advance"
        ))
    })?;
    let now_ts = chrono::Utc::now()
        .timestamp_micros()
        .max(minimum_updated_at_micros);

    // Check for a conflicting canonical row (same namespace + natural key,
    // different id). This catches conflicts whether or not endpoints were flipped.
    let conflict_id: Option<String> = conn
        .query_row(
            EDGE_SYMMETRIC_CONFLICT_PROBE_SQL,
            rusqlite::params![
                &ns,
                &canon_src_str,
                &canon_tgt_str,
                &relation_str,
                &edge_id_str
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(SqliteError::Rusqlite)?;

    if let Some(existing_id) = conflict_id {
        // Case (b): canonical row already exists — ADR-039's edge-conflict
        // contract is ON CONFLICT DO NOTHING: drop the non-canonical edge
        // and leave the existing canonical row untouched (live or
        // tombstoned). Refreshing it from the discarded edge's
        // weight/target_backend/metadata and forcing deleted_at = NULL
        // would silently overwrite the survivor and resurrect a
        // tombstone — the same defect already fixed on the merge-rewire
        // path (`merge_entity_sql`/`merge_note_sql`); this path binds the
        // same shared `EDGE_SYMMETRIC_*_SQL` text and must honor the same
        // contract. Return the surviving id unchanged so the caller
        // re-fetches its real (unmodified) attributes.
        //
        // Guarded on the fetched snapshot's revision and deletion marker:
        // a concurrent writer that changed this edge between fetch and
        // this write must be refused, not silently deleted just because
        // a canonical survivor happens to exist. Zero affected rows here
        // means stale, not "no conflict" — the probe above already
        // confirmed a conflicting canonical row exists.
        let affected = conn
            .execute(
                EDGE_SYMMETRIC_DELETE_NONCANONICAL_GUARDED_SQL,
                rusqlite::params![
                    &ns,
                    &edge_id_str,
                    expected_updated_at_micros,
                    expected_deleted_at_micros,
                ],
            )
            .map_err(SqliteError::Rusqlite)?;
        if affected == 0 {
            return Ok(SymmetricEdgeUpdateOutcome::Stale);
        }
        Ok(SymmetricEdgeUpdateOutcome::Absorbed(existing_id))
    } else {
        // Case (a): no conflict — update source_id/target_id in-place,
        // preserving the original edge UUID. Guarded on the fetched
        // snapshot's revision and deletion marker: a concurrent writer
        // that moved this edge between fetch and this write must be
        // refused, not silently overwritten by a stale full-row update.
        let affected = conn
            .execute(
                EDGE_SYMMETRIC_UPDATE_INPLACE_SQL,
                rusqlite::params![
                    &canon_src_str,
                    &canon_tgt_str,
                    &relation_str,
                    weight,
                    now_ts,
                    metadata,
                    &ns,
                    &edge_id_str,
                    expected_updated_at_micros,
                    expected_deleted_at_micros,
                ],
            )
            .map_err(SqliteError::Rusqlite)?;
        if affected == 0 {
            return Ok(SymmetricEdgeUpdateOutcome::Stale);
        }
        Ok(SymmetricEdgeUpdateOutcome::Updated)
    }
}

#[cfg(test)]
#[path = "symmetric_update/tests.rs"]
mod tests;
