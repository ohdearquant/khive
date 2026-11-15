use khive_storage::graph::{
    EdgeInsertIfAbsentOutcome, GraphGuardValueKind, GraphStatementGuard,
    GraphStatementGuardFailure, GraphStatementGuardFailureReason,
};
use khive_storage::{Edge, StorageCapability, StorageError, StorageResult};
use rusqlite::types::ValueRef;

use super::edge_statements::{canonical_edge_endpoints, edge_insert_if_absent_statement};
use super::edge_upserts::edge_by_natural_key_including_deleted;
use super::map_err;
use super::mutation_plans::GRAPH_MUTATION_EVENTS_OP;
use crate::sql_bridge::{bind_params, statement_guard_select_admits};

pub(super) fn check_statement_guards(
    conn: &rusqlite::Connection,
    guards: &[GraphStatementGuard],
) -> StorageResult<Option<GraphStatementGuardFailure>> {
    for guard in guards {
        statement_guard_select_admits(&guard.statement.sql).map_err(|message| {
            StorageError::InvalidInput {
                capability: StorageCapability::Graph,
                operation: GRAPH_MUTATION_EVENTS_OP.into(),
                message,
            }
        })?;
        // prepare rejects executable tails; admission above also excludes
        // connection-setting statement heads that readonly() alone permits.
        let mut statement = conn
            .prepare(&guard.statement.sql)
            .map_err(|error| map_err(error, GRAPH_MUTATION_EVENTS_OP))?;
        if !statement.readonly() {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Graph,
                operation: GRAPH_MUTATION_EVENTS_OP.into(),
                message: "graph statement guards require a read-only SELECT".into(),
            });
        }
        let failure = |reason| {
            Some(GraphStatementGuardFailure {
                label: guard.label.clone(),
                reason,
            })
        };
        let column_count = statement.column_count();
        if column_count != 1 {
            return Ok(failure(GraphStatementGuardFailureReason::ColumnCount {
                actual: column_count,
            }));
        }
        bind_params(&mut statement, &guard.statement.params)
            .map_err(|error| map_err(error, GRAPH_MUTATION_EVENTS_OP))?;
        let mut rows = statement.raw_query();
        let Some(row) = rows
            .next()
            .map_err(|error| map_err(error, GRAPH_MUTATION_EVENTS_OP))?
        else {
            return Ok(failure(GraphStatementGuardFailureReason::NoRows));
        };
        let reason = match row
            .get_ref(0)
            .map_err(|error| map_err(error, GRAPH_MUTATION_EVENTS_OP))?
        {
            ValueRef::Integer(1) => None,
            ValueRef::Integer(0) => Some(GraphStatementGuardFailureReason::PredicateFalse),
            value => Some(GraphStatementGuardFailureReason::NonBoolean {
                kind: match value {
                    ValueRef::Null => GraphGuardValueKind::Null,
                    ValueRef::Integer(_) => GraphGuardValueKind::Integer,
                    ValueRef::Real(_) => GraphGuardValueKind::Real,
                    ValueRef::Text(_) => GraphGuardValueKind::Text,
                    ValueRef::Blob(_) => GraphGuardValueKind::Blob,
                },
            }),
        };
        // Cardinality wins over a false or non-boolean first value.
        if rows
            .next()
            .map_err(|error| map_err(error, GRAPH_MUTATION_EVENTS_OP))?
            .is_some()
        {
            return Ok(failure(GraphStatementGuardFailureReason::MultipleRows));
        }
        if let Some(reason) = reason {
            return Ok(failure(reason));
        }
    }
    Ok(None)
}

pub(super) fn insert_edge_if_absent(
    conn: &rusqlite::Connection,
    mut edge: Edge,
) -> Result<EdgeInsertIfAbsentOutcome, rusqlite::Error> {
    (edge.source_id, edge.target_id) =
        canonical_edge_endpoints(edge.relation, edge.source_id, edge.target_id);
    let insert = edge_insert_if_absent_statement(&edge);
    let mut statement = conn.prepare_cached(&insert.sql)?;
    bind_params(&mut statement, &insert.params)?;
    if statement.raw_execute()? == 1 {
        return Ok(EdgeInsertIfAbsentOutcome::Created(edge));
    }
    // Match the legacy conditional insert's natural-key precedence even if
    // the supplied ID also collides with a different row.
    match edge_by_natural_key_including_deleted(conn, &edge)? {
        Some(existing) if existing.deleted_at.is_some() => {
            Ok(EdgeInsertIfAbsentOutcome::Tombstoned)
        }
        Some(_) => Ok(EdgeInsertIfAbsentOutcome::ExistingLive),
        None => Err(rusqlite::Error::QueryReturnedNoRows),
    }
}
