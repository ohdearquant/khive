use super::{
    delete_document_statements, event_insert_statements, EventKind, KhiveRuntime, NamespaceToken,
    PlanStatement, RuntimeError, RuntimeResult, SqlStatement, SqlValue, SubstrateKind, Uuid, Value,
};

#[cfg(doc)]
use crate::atomic_runner::apply_plan;

/// Every registered embedding model's vector table name, in the exact format
/// `curation::merge_entity_sql` uses (`"vec_{sanitize_key(model_name)}"`) —
/// reused here so atomic delete/merge purge the same tables the non-atomic
/// paths do.
fn vector_table_names(runtime: &KhiveRuntime) -> Vec<String> {
    runtime
        .registered_embedding_model_names()
        .iter()
        .map(|name| format!("vec_{}", crate::config::sanitize_key(name)))
        .collect()
}

/// A guarded (`guard: None` — best-effort mirror, matching the non-atomic
/// index-cleanup calls which don't assert a row existed) `DELETE` statement
/// against one vector table for a single subject, scoped by namespace.
///
/// Vector tables carry a real index on `(subject_id, namespace)`
/// ([`khive_db::stores::vectors`]) — this row-scan predicate is not the FTS
/// full-table-scan class this module's `purge_fts_document_statements`
/// exists to avoid, so it is left as a direct `namespace = ? AND subject_id =
/// ?` predicate.
fn purge_index_row_statement(
    table: &str,
    namespace: &str,
    subject_id: Uuid,
    label: &str,
) -> PlanStatement {
    PlanStatement {
        statement: SqlStatement {
            sql: format!("DELETE FROM {table} WHERE namespace = ?1 AND subject_id = ?2"),
            params: vec![
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(subject_id.to_string()),
            ],
            label: Some(label.to_string()),
        },
        guard: None,
    }
}

fn purge_vector_provenance_statement(
    table: &str,
    namespace: &str,
    subject_id: Uuid,
    label: &str,
) -> PlanStatement {
    let model_key = table
        .strip_prefix("vec_")
        .expect("runtime vector tables use the vec_ prefix");
    PlanStatement {
        statement: SqlStatement {
            sql: "DELETE FROM vector_provenance \
                  WHERE model_key = ?1 AND subject_id = ?2 AND namespace = ?3"
                .to_string(),
            params: vec![
                SqlValue::Text(model_key.to_string()),
                SqlValue::Text(subject_id.to_string()),
                SqlValue::Text(namespace.to_string()),
            ],
            label: Some(label.to_string()),
        },
        guard: None,
    }
}

/// The FTS-document half of an index purge: `fts_table`'s row for `subject_id`
/// (looked up via `khive_db::stores::text::rowid_map_table`, not a
/// `namespace`/`subject_id` scan — those columns are `UNINDEXED` in every
/// FTS5 DDL) plus that row's own entry in the sidecar map. Order-sensitive:
/// index 0 must run before index 1 — see `delete_document_statements`'s
/// adjacency contract.
fn purge_fts_document_statements(
    fts_table: &str,
    namespace: &str,
    subject_id: Uuid,
    label_prefix: &str,
) -> [PlanStatement; 2] {
    let [mut fts_stmt, mut map_stmt] = delete_document_statements(fts_table, namespace, subject_id);
    fts_stmt.label = Some(label_prefix.to_string());
    map_stmt.label = Some(format!("{label_prefix}-map"));
    [
        PlanStatement {
            statement: fts_stmt,
            guard: None,
        },
        PlanStatement {
            statement: map_stmt,
            guard: None,
        },
    ]
}

fn log_vector_row_delete_statement(
    table: &str,
    namespace: &str,
    subject_id: Uuid,
    label: &str,
) -> PlanStatement {
    PlanStatement {
        statement: SqlStatement {
            sql: format!(
                "INSERT INTO ann_write_log \
                 (namespace, embedding_model, kind, field, subject_id, op) \
                 SELECT namespace, embedding_model, kind, field, subject_id, 'delete' \
                 FROM {table} WHERE namespace = ?1 AND subject_id = ?2"
            ),
            params: vec![
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(subject_id.to_string()),
            ],
            label: Some(label.to_string()),
        },
        guard: None,
    }
}

/// `true` iff a table named `table` currently exists in the backing SQLite
/// database (`sqlite_master` probe, read-only — safe in async prepare, does
/// NOT open/create the vector store, so it cannot lazily create the table
/// itself).
async fn vector_table_exists(runtime: &KhiveRuntime, table: &str) -> RuntimeResult<bool> {
    let mut reader = runtime
        .sql()
        .reader()
        .await
        .map_err(RuntimeError::Storage)?;
    let row = reader
        .query_scalar(SqlStatement {
            sql: "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1".to_string(),
            params: vec![SqlValue::Text(table.to_string())],
            label: Some("atomic-delete-vec-table-exists".to_string()),
        })
        .await
        .map_err(RuntimeError::Storage)?;
    Ok(row.is_some())
}

/// Append the FTS + every registered model's vector-row purge for `subject_id`
/// (scoped to the RECORD's own namespace, matching `delete_entity`/
/// `delete_note`'s `record_tok`/`record_ns` convention: not the caller
/// token's namespace, per by-ID namespace-agnosticism) onto `statements`.
///
/// FTS tables (`fts_entities`/`fts_notes`) always exist (created at schema
/// migration time) so their purge is unconditional. `vec_*` tables are
/// created lazily on first vector-store open, so a default runtime can
/// register embedding models before any vector table necessarily exists:
/// a raw unconditional `DELETE FROM vec_*` can hit `no such table` on a
/// fresh DB. Only push the vec purge for tables that actually exist:
/// absence means the record definitionally has no vector row for that
/// model, so skipping is data-parity-correct (the non-atomic path would
/// lazily create the table then delete zero rows: same data outcome,
/// without this read-only prepare pass performing an init side effect).
pub(super) async fn push_index_purge_statements(
    runtime: &KhiveRuntime,
    statements: &mut Vec<PlanStatement>,
    fts_table: &str,
    namespace: &str,
    subject_id: Uuid,
    label_prefix: &str,
) -> RuntimeResult<()> {
    statements.extend(purge_fts_document_statements(
        fts_table,
        namespace,
        subject_id,
        &format!("{label_prefix}-purge-fts"),
    ));
    for vec_table in vector_table_names(runtime) {
        if vector_table_exists(runtime, &vec_table).await? {
            statements.push(log_vector_row_delete_statement(
                &vec_table,
                namespace,
                subject_id,
                &format!("{label_prefix}-log-delete-vec-{vec_table}"),
            ));
            statements.push(purge_index_row_statement(
                &vec_table,
                namespace,
                subject_id,
                &format!("{label_prefix}-purge-vec-{vec_table}"),
            ));
            statements.push(purge_vector_provenance_statement(
                &vec_table,
                namespace,
                subject_id,
                &format!("{label_prefix}-purge-vec-provenance-{vec_table}"),
            ));
        }
    }
    Ok(())
}

/// Event-store append parity for the canonical handlers that emit a
/// lifecycle event after their row mutation: `update_entity` ->
/// `EntityUpdated`, `delete_entity` -> `EntityDeleted`, `delete_note` ->
/// `NoteDeleted`, `update_edge` -> `EdgeUpdated`, `delete_edge` ->
/// `EdgeDeleted`, `link` -> `LinkCreated`/`EdgeUpdated`, and
/// `update_note` -> `NoteUpdated`. See
/// `docs/api/atomic_prepare.md#event_append_statements` for why
/// this is a `PlanStatement` rather than a `PostCommitEffect`.
///
/// Invariant: returned statements are unguarded — appended after the plan's
/// own guarded row statement, so [`apply_plan`]'s stop-on-first-failure
/// contract means they are only reached once that row mutation's guard has
/// already held. Committing the event row atomically with the mutation it
/// describes strengthens canonical's guarantee: the non-atomic handlers write
/// the event in a separate transaction, ordered but not atomic with the row
/// mutation.
pub(crate) fn event_append_statements(
    token: &NamespaceToken,
    namespace: &str,
    verb: &str,
    kind: EventKind,
    substrate: SubstrateKind,
    target_id: Uuid,
    payload: Value,
) -> RuntimeResult<Vec<PlanStatement>> {
    let record_token = token
        .with_namespace(crate::Namespace::parse(namespace).map_err(|error| {
            RuntimeError::Internal(format!("event namespace invalid: {error}"))
        })?);
    let event = crate::EventAttribution::from_token(&record_token).stamp(
        khive_storage::event::Event::new(namespace.to_string(), verb, kind, substrate, "")
            .with_target(target_id)
            .with_payload(payload),
    );
    let statements = event_insert_statements(&event)
        .map_err(|e| RuntimeError::Internal(format!("event_insert_statements: {e}")))?;
    Ok(statements
        .into_iter()
        .map(|statement| PlanStatement {
            statement,
            guard: None,
        })
        .collect())
}
