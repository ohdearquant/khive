use super::*;

#[allow(clippy::too_many_arguments)]
pub fn edge_symmetric_delete_if_conflict_statement(
    namespace: &str,
    id: Uuid,
    canon_src: Uuid,
    canon_tgt: Uuid,
    relation: EdgeRelation,
    expected_updated_at_micros: i64,
    expected_deleted_at_micros: Option<i64>,
) -> SqlStatement {
    SqlStatement {
        sql: "DELETE FROM graph_edges \
              WHERE namespace = ?1 AND id = ?2 \
                AND updated_at = ?6 AND deleted_at IS ?7 \
                AND EXISTS ( \
                  SELECT 1 FROM graph_edges \
                  WHERE namespace = ?1 AND source_id = ?3 AND target_id = ?4 \
                    AND relation = ?5 AND id != ?2 \
                )"
        .to_string(),
        params: vec![
            SqlValue::Text(namespace.to_string()),
            SqlValue::Text(id.to_string()),
            SqlValue::Text(canon_src.to_string()),
            SqlValue::Text(canon_tgt.to_string()),
            SqlValue::Text(relation.to_string()),
            SqlValue::Integer(expected_updated_at_micros),
            match expected_deleted_at_micros {
                Some(value) => SqlValue::Integer(value),
                None => SqlValue::Null,
            },
        ],
        label: Some("edge-symmetric-delete-if-conflict".to_string()),
    }
}

/// `?10`/`?11` pin the fetched snapshot's `updated_at`/`deleted_at` and
/// `?7 > updated_at` requires the replacement revision to strictly advance —
/// guarding the in-place arm (`id = ?2`) against a concurrent writer that
/// changed the row between the snapshot read and this statement. The
/// absorbed arm (a differently-id'd canonical row) is intentionally left
/// unguarded on revision: per ADR-039 DO NOTHING it self-assigns every
/// column, so it is a no-op write regardless of the survivor's current
/// state.
#[allow(clippy::too_many_arguments)]
pub fn edge_symmetric_absorb_or_update_inplace_statement(
    namespace: &str,
    id: Uuid,
    canon_src: Uuid,
    canon_tgt: Uuid,
    relation: EdgeRelation,
    weight: f64,
    updated_at_micros: i64,
    metadata: Option<&str>,
    target_backend: Option<&str>,
    expected_updated_at_micros: i64,
    expected_deleted_at_micros: Option<i64>,
) -> SqlStatement {
    SqlStatement {
        sql: "UPDATE graph_edges SET \
              source_id = CASE WHEN id = ?2 THEN ?3 ELSE source_id END, \
              target_id = CASE WHEN id = ?2 THEN ?4 ELSE target_id END, \
              relation = CASE WHEN id = ?2 THEN ?5 ELSE relation END, \
              weight = CASE WHEN id = ?2 THEN ?6 ELSE weight END, \
              updated_at = CASE WHEN id = ?2 THEN ?7 ELSE updated_at END, \
              deleted_at = CASE WHEN id = ?2 THEN NULL ELSE deleted_at END, \
              metadata = CASE WHEN id = ?2 THEN ?8 ELSE metadata END, \
              target_backend = CASE WHEN id = ?2 THEN ?9 ELSE target_backend END \
              WHERE namespace = ?1 \
                AND ( \
                  (id = ?2 AND changes() = 0 AND updated_at = ?10 AND deleted_at IS ?11 \
                      AND ?7 > updated_at) \
                  OR (source_id = ?3 AND target_id = ?4 AND relation = ?5 \
                      AND id != ?2 AND changes() = 1) \
                )"
        .to_string(),
        params: vec![
            SqlValue::Text(namespace.to_string()),
            SqlValue::Text(id.to_string()),
            SqlValue::Text(canon_src.to_string()),
            SqlValue::Text(canon_tgt.to_string()),
            SqlValue::Text(relation.to_string()),
            SqlValue::Float(weight),
            SqlValue::Integer(updated_at_micros),
            match metadata {
                Some(m) => SqlValue::Text(m.to_string()),
                None => SqlValue::Null,
            },
            match target_backend {
                Some(b) => SqlValue::Text(b.to_string()),
                None => SqlValue::Null,
            },
            SqlValue::Integer(expected_updated_at_micros),
            match expected_deleted_at_micros {
                Some(value) => SqlValue::Integer(value),
                None => SqlValue::Null,
            },
        ],
        label: Some("edge-symmetric-absorb-or-update-inplace".to_string()),
    }
}

/// Internal seam for khive-runtime; no compatibility promise.
///
/// A live source-document reference observed before pack preparation.
#[doc(hidden)]
pub struct GraphDocumentGuard {
    pub namespace: String,
    pub id: Uuid,
    pub expected_blob_ref: String,
}

/// Internal seam for khive-runtime; no compatibility promise.
///
/// The complete row, or absence, observed for one canonical natural key.
#[doc(hidden)]
pub struct GraphEdgeSnapshotGuard {
    pub namespace: String,
    pub source_id: Uuid,
    pub target_id: Uuid,
    pub relation: EdgeRelation,
    pub expected: Option<Edge>,
}

/// Internal seam for khive-runtime; no compatibility promise.
///
/// Source-row expectations checked before any graph mutation.
#[doc(hidden)]
#[derive(Default)]
pub struct GraphMutationPreconditions {
    pub document: Option<GraphDocumentGuard>,
    pub edges: Vec<GraphEdgeSnapshotGuard>,
    pub statement_guards: Vec<GraphStatementGuard>,
}

/// Internal seam for khive-runtime; no compatibility promise.
///
/// Existing graph engines selected for a source-backend composition unit.
#[doc(hidden)]
pub enum GraphMutationRequest {
    Single {
        request: EdgeUpsertRequest,
        guard_endpoints: bool,
    },
    Batch {
        requests: Vec<EdgeUpsertRequest>,
        guard_endpoints: bool,
    },
    InsertIfAbsent {
        edge: Edge,
    },
    CommitAnnotation {
        edge: Edge,
        guard: CommitAnnotationGuard,
    },
}

/// Internal seam for khive-runtime; no compatibility promise.
///
/// Transaction-observed outcomes retain the existing graph classifications.
#[doc(hidden)]
pub enum GraphMutationOutcome {
    Single(GuardedEdgeUpsertOutcome),
    Batch(GuardedEdgeBatchUpsertOutcome),
    /// This arm carries only Created, ExistingLive or Tombstoned. Statement
    /// guard refusals use the top-level arm for every mutation request kind.
    InsertIfAbsent(EdgeInsertIfAbsentOutcome),
    GuardFailed(GraphStatementGuardFailure),
    CommitAnnotation(CommitAnnotationInsertOutcome),
}

/// Internal seam for khive-runtime; no compatibility promise.
///
/// Ordered upsert results and the separate preimages of retired live edges.
#[doc(hidden)]
pub struct GraphMutationEventOutcome {
    pub mutation: GraphMutationOutcome,
    pub retired: Vec<Edge>,
}

pub(super) const GRAPH_MUTATION_EVENTS_OP: &str = "compose_graph_mutation_events";

pub(super) fn count_graph_mutation_events(outcome: &GraphMutationEventOutcome) {
    let written = match &outcome.mutation {
        GraphMutationOutcome::Single(GuardedEdgeUpsertOutcome::Written(_)) => 1,
        GraphMutationOutcome::Batch(outcome) if outcome.refusal.is_none() => outcome.rows.len(),
        GraphMutationOutcome::InsertIfAbsent(EdgeInsertIfAbsentOutcome::Created(_)) => 1,
        GraphMutationOutcome::CommitAnnotation(CommitAnnotationInsertOutcome::Created(_)) => 1,
        _ => 0,
    };
    khive_storage::usage::count(
        khive_storage::usage::UsageUnit::EventRows,
        (written + outcome.retired.len()) as u64,
    );
}

/// Internal seam for khive-runtime; no compatibility promise.
///
/// Guarded composition seam for packs.
#[doc(hidden)]
pub async fn compose_graph_mutation_events<F>(
    backend: &crate::StorageBackend,
    mutation: GraphMutationRequest,
    preconditions: GraphMutationPreconditions,
    retirements: Vec<Edge>,
    make_events: F,
) -> StorageResult<GraphMutationEventOutcome>
where
    F: FnOnce(&GraphMutationEventOutcome) -> StorageResult<Vec<Event>> + Send + 'static,
{
    if backend.is_read_only() {
        return Err(StorageError::Pool {
            operation: GRAPH_MUTATION_EVENTS_OP.into(),
            message: "backend is read-only".into(),
        });
    }
    // Retain the normal accessors' store-readiness checks before admission;
    // no schema work or asynchronous dispatch enters the enlisted engine.
    backend
        .graph()
        .map_err(|error| map_sqlite_err(error, GRAPH_MUTATION_EVENTS_OP))?;
    backend
        .events()
        .map_err(|error| map_sqlite_err(error, GRAPH_MUTATION_EVENTS_OP))?;
    let pool = backend.pool_arc();
    if let Some(writer_task) = pool.writer_task_for_write(None, GRAPH_MUTATION_EVENTS_OP)? {
        return writer_task
            .send_bounded_at(
                move |conn| {
                    graph_mutation_events_enlisted(
                        conn,
                        mutation,
                        preconditions,
                        retirements,
                        make_events,
                    )
                },
                GRAPH_MUTATION_EVENTS_OP,
            )
            .await
            .inspect(count_graph_mutation_events)
            .inspect_err(|error| khive_storage::usage::account_event_write(Err(error)));
    }
    pool.record_direct_route(crate::timeout_sink::Site::DirectRouteGraphGeneralWrite);
    let is_file_backed = backend.is_file_backed();
    tokio::task::spawn_blocking(move || {
        if is_file_backed {
            let admission = pool.write_admission();
            let _lease = admission
                .acquire()
                .map_err(|error| map_sqlite_err(error, GRAPH_MUTATION_EVENTS_OP))?;
            let conn = pool
                .open_standalone_writer_for_admitted_operation()
                .map_err(|error| map_sqlite_err(error, GRAPH_MUTATION_EVENTS_OP))?;
            run_graph_mutation_transaction(&pool, &conn, false, move |conn| {
                graph_mutation_events_enlisted(
                    conn,
                    mutation,
                    preconditions,
                    retirements,
                    make_events,
                )
            })
        } else {
            let guard = pool
                .writer_for_admitted_operation()
                .map_err(|error| map_sqlite_err(error, GRAPH_MUTATION_EVENTS_OP))?;
            run_graph_mutation_transaction(&pool, guard.conn(), true, move |conn| {
                graph_mutation_events_enlisted(
                    conn,
                    mutation,
                    preconditions,
                    retirements,
                    make_events,
                )
            })
        }
    })
    .await
    .map_err(|error| {
        StorageError::driver(StorageCapability::Graph, GRAPH_MUTATION_EVENTS_OP, error)
    })?
    .inspect(count_graph_mutation_events)
    .inspect_err(|error| khive_storage::usage::account_event_write(Err(error)))
}

pub(super) fn graph_mutation_conflict(message: &'static str) -> StorageError {
    StorageError::Conflict {
        capability: StorageCapability::Graph,
        operation: GRAPH_MUTATION_EVENTS_OP.into(),
        message: message.into(),
    }
}

pub(super) fn edge_snapshot_matches(actual: &Edge, expected: &Edge) -> bool {
    actual.id == expected.id
        && actual.namespace == expected.namespace
        && actual.source_id == expected.source_id
        && actual.target_id == expected.target_id
        && actual.relation == expected.relation
        && actual.weight.to_bits() == expected.weight.to_bits()
        && actual.metadata == expected.metadata
        && actual.target_backend == expected.target_backend
        && actual.created_at == expected.created_at
        && actual.updated_at == expected.updated_at
        && actual.deleted_at == expected.deleted_at
}

pub(super) fn check_graph_mutation_preconditions(
    conn: &rusqlite::Connection,
    preconditions: &GraphMutationPreconditions,
    retirements: &[Edge],
) -> StorageResult<()> {
    if let Some(document) = &preconditions.document {
        let matches: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM entities WHERE id=?1 AND namespace=?2 \
                 AND deleted_at IS NULL AND json_type(properties, '$.blob_ref')='text' \
                 AND json_extract(properties, '$.blob_ref')=?3 COLLATE BINARY)",
                rusqlite::params![
                    document.id.to_string(),
                    &document.namespace,
                    &document.expected_blob_ref,
                ],
                |row| row.get(0),
            )
            .map_err(|error| map_err(error, GRAPH_MUTATION_EVENTS_OP))?;
        if !matches {
            return Err(graph_mutation_conflict(
                "source document body reference changed",
            ));
        }
    }
    for guard in &preconditions.edges {
        let actual = edge_by_natural_key_parts_including_deleted(
            conn,
            &guard.namespace,
            guard.source_id,
            guard.target_id,
            guard.relation,
        )
        .map_err(|error| map_err(error, GRAPH_MUTATION_EVENTS_OP))?;
        let matches = match (actual.as_ref(), guard.expected.as_ref()) {
            (None, None) => true,
            (Some(actual), Some(expected)) => edge_snapshot_matches(actual, expected),
            _ => false,
        };
        if !matches {
            return Err(graph_mutation_conflict("edge ownership snapshot changed"));
        }
    }
    for expected in retirements {
        let actual = edge_by_natural_key_including_deleted(conn, expected)
            .map_err(|error| map_err(error, GRAPH_MUTATION_EVENTS_OP))?;
        if expected.deleted_at.is_some()
            || !actual
                .as_ref()
                .is_some_and(|actual| edge_snapshot_matches(actual, expected))
        {
            return Err(graph_mutation_conflict("edge retirement snapshot changed"));
        }
    }
    Ok(())
}

/// Connection-enlisted graph/event engine: no transaction control or await.
pub(super) fn graph_mutation_events_enlisted<F>(
    conn: &rusqlite::Connection,
    mutation: GraphMutationRequest,
    preconditions: GraphMutationPreconditions,
    retirements: Vec<Edge>,
    make_events: F,
) -> StorageResult<GraphMutationEventOutcome>
where
    F: FnOnce(&GraphMutationEventOutcome) -> StorageResult<Vec<Event>>,
{
    if !retirements.is_empty() && !matches!(&mutation, GraphMutationRequest::Batch { .. }) {
        return Err(StorageError::InvalidInput {
            capability: StorageCapability::Graph,
            operation: GRAPH_MUTATION_EVENTS_OP.into(),
            message: "retirements require a batch mutation".into(),
        });
    }
    check_graph_mutation_preconditions(conn, &preconditions, &retirements)?;
    if let Some(failure) = check_statement_guards(conn, &preconditions.statement_guards)? {
        return Ok(GraphMutationEventOutcome {
            mutation: GraphMutationOutcome::GuardFailed(failure),
            retired: Vec::new(),
        });
    }
    let mutation = match mutation {
        GraphMutationRequest::Single {
            request,
            guard_endpoints,
        } => GraphMutationOutcome::Single(
            observed_edge_upsert(conn, &request, guard_endpoints)
                .map_err(|error| map_err(error, GRAPH_MUTATION_EVENTS_OP))?,
        ),
        GraphMutationRequest::Batch {
            requests,
            guard_endpoints,
        } => {
            let outcome = observed_edge_batch_upsert(conn, &requests, guard_endpoints)
                .map_err(|error| map_err(error, GRAPH_MUTATION_EVENTS_OP))?;
            if outcome.refusal.is_none() && outcome.rows.len() != requests.len() {
                return Err(graph_mutation_conflict(
                    "edge result count differs from request count",
                ));
            }
            GraphMutationOutcome::Batch(outcome)
        }
        GraphMutationRequest::InsertIfAbsent { edge } => {
            if edge.deleted_at.is_some() {
                return Err(StorageError::InvalidInput {
                    capability: StorageCapability::Graph,
                    operation: GRAPH_MUTATION_EVENTS_OP.into(),
                    message: "expected a live edge for create-only insertion".into(),
                });
            }
            #[cfg(test)]
            statement_guard_tests::after_guards(&edge);
            GraphMutationOutcome::InsertIfAbsent(
                insert_edge_if_absent(conn, edge)
                    .map_err(|error| map_err(error, GRAPH_MUTATION_EVENTS_OP))?,
            )
        }
        GraphMutationRequest::CommitAnnotation { edge, guard } => {
            if edge.relation != EdgeRelation::Annotates || edge.deleted_at.is_some() {
                return Err(StorageError::InvalidInput {
                    capability: StorageCapability::Graph,
                    operation: GRAPH_MUTATION_EVENTS_OP.into(),
                    message: "expected a live annotates edge".into(),
                });
            }
            GraphMutationOutcome::CommitAnnotation(
                conditional_commit_annotation_insert(conn, edge, &guard)
                    .map_err(|error| map_err(error, GRAPH_MUTATION_EVENTS_OP))?,
            )
        }
    };
    let written = match &mutation {
        GraphMutationOutcome::Single(GuardedEdgeUpsertOutcome::Written(_)) => 1,
        GraphMutationOutcome::Batch(outcome) if outcome.refusal.is_none() => outcome.rows.len(),
        GraphMutationOutcome::InsertIfAbsent(EdgeInsertIfAbsentOutcome::Created(_)) => 1,
        GraphMutationOutcome::CommitAnnotation(CommitAnnotationInsertOutcome::Created(_)) => 1,
        _ => {
            return Ok(GraphMutationEventOutcome {
                mutation,
                retired: Vec::new(),
            })
        }
    };
    let mut retired = Vec::with_capacity(retirements.len());
    for edge in retirements {
        let statement =
            edge_soft_delete_statement(Uuid::from(edge.id), Utc::now().timestamp_micros());
        let mut stmt = conn
            .prepare(&statement.sql)
            .map_err(|error| map_err(error, GRAPH_MUTATION_EVENTS_OP))?;
        bind_params(&mut stmt, &statement.params)
            .map_err(|error| map_err(error, GRAPH_MUTATION_EVENTS_OP))?;
        if stmt
            .raw_execute()
            .map_err(|error| map_err(error, GRAPH_MUTATION_EVENTS_OP))?
            != 1
        {
            return Err(graph_mutation_conflict(
                "edge retirement changed during mutation",
            ));
        }
        retired.push(edge);
    }
    let outcome = GraphMutationEventOutcome { mutation, retired };
    let expected_events = written + outcome.retired.len();
    if expected_events == 0 {
        return Ok(outcome);
    }
    let events = make_events(&outcome)?;
    if events.len() != expected_events {
        return Err(graph_mutation_conflict(
            "event result count differs from mutation count",
        ));
    }
    for event in &events {
        super::event::append_event_in_transaction(conn, event).map_err(|error| {
            StorageError::driver(StorageCapability::Events, GRAPH_MUTATION_EVENTS_OP, error)
        })?;
    }
    Ok(outcome)
}
