//! Prefix removal uses one SQL-only writer and the canonical note-delete plan.
use khive_storage::{SqlStatement, SqlValue, StorageError};
use serde_json::{json, Value};
use uuid::Uuid;

use super::{stream_position_statement, validate_stream};
use crate::atomic_prepare::{
    apply_post_commit_effects_with_report, note_delete_plan, push_index_purge_for_tables,
    vector_table_names, vector_tables_in_writer,
};
use crate::atomic_runner::{
    apply_plan_preserving_storage, run_prepared_atomic_unit, PlanExecutionFailure,
    PreparedAtomicError, PreparedAtomicOp, PreparedAtomicOutcome,
};
use crate::{KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};

fn refused(error: RuntimeError) -> PreparedAtomicError<RuntimeError> {
    PreparedAtomicError::Refused {
        message: error.to_string(),
        failure: error,
    }
}

fn invalid_state(error: impl std::fmt::Display) -> PreparedAtomicError<RuntimeError> {
    refused(RuntimeError::Internal(format!(
        "stream truncation: {error}"
    )))
}

impl KhiveRuntime {
    /// Remove entries below the clipped prefix bound, retaining a monotonic head.
    /// All ledger, note, index and floor writes share the same transaction.
    pub async fn stream_truncate(
        &self,
        token: &NamespaceToken,
        stream: &str,
        before_seq: i64,
    ) -> RuntimeResult<Value> {
        validate_stream(stream)?;
        // Open stores before entering the suspend-free applying transaction.
        self.notes(token)?;
        self.graph(token)?;
        self.text_for_notes(token)?;
        self.events(token)?;
        let vector_candidates = vector_table_names(self);
        let applying_token = token.clone();
        let stream = stream.to_owned();
        let op: PreparedAtomicOp<Value, RuntimeError> = Box::new(move |writer| {
            Box::pin(async move {
                let namespace = applying_token.namespace().as_str();
                let position = writer
                    .query_row(stream_position_statement(namespace, &stream))
                    .await?
                    .ok_or_else(|| invalid_state("missing stream position"))?;
                let head = position.i64("head_seq").map_err(invalid_state)?;
                let floor = position.i64("floor_seq").map_err(invalid_state)?;
                let next = head
                    .checked_add(1)
                    .ok_or_else(|| invalid_state("unrepresentable stream floor"))?;
                let before = before_seq.min(next);
                if before <= floor {
                    return Ok((
                        json!({"removed": 0, "floor_seq": floor, "head_seq": head}),
                        vec![],
                    ));
                }
                let prefix_params = vec![
                    SqlValue::Text(namespace.into()),
                    SqlValue::Text(stream.clone()),
                    SqlValue::Integer(before),
                ];
                let rows = writer
                    .query_all(SqlStatement::new(
                        "SELECT n.id,n.kind FROM note_streams s JOIN notes n ON n.id=s.note_id AND n.namespace=s.namespace \
                         WHERE s.namespace=?1 AND s.stream=?2 AND s.seq<?3 ORDER BY s.seq",
                        prefix_params.clone(),
                    ).labelled("stream-truncate-members"))
                    .await?;
                let tables = vector_tables_in_writer(writer, &vector_candidates).await?;
                let permit = writer
                    .execute(SqlStatement::new(
                        "INSERT INTO note_stream_truncations(namespace,stream,before_seq) VALUES (?1,?2,?3)",
                        prefix_params.clone(),
                    ).labelled("stream-truncate-permit"))
                    .await?;
                if permit != 1 {
                    return Err(invalid_state("truncation permit was not inserted"));
                }
                let removed = writer
                    .execute(
                        SqlStatement::new(
                            "DELETE FROM note_streams WHERE namespace=?1 AND stream=?2 AND seq<?3",
                            prefix_params.clone(),
                        )
                        .labelled("stream-truncate-ledger"),
                    )
                    .await?;
                if removed != rows.len() as u64 {
                    return Err(invalid_state("ledger prefix has missing note records"));
                }
                let mut effects = Vec::new();
                for row in rows {
                    let id = Uuid::parse_str(row.text("id").map_err(invalid_state)?)
                        .map_err(invalid_state)?;
                    let kind = row.text("kind").map_err(invalid_state)?;
                    let mut indexes = Vec::new();
                    push_index_purge_for_tables(
                        &mut indexes,
                        "fts_notes",
                        namespace,
                        id,
                        "stream-truncate-note",
                        &tables,
                    );
                    let plan =
                        note_delete_plan(&applying_token, id, namespace, kind, true, indexes)
                            .map_err(refused)?;
                    match apply_plan_preserving_storage(writer, &plan, false).await {
                        Ok(applied) => effects.extend(applied.effect),
                        Err(PlanExecutionFailure::Storage { source, .. }) => {
                            return Err(PreparedAtomicError::Storage(source));
                        }
                        Err(PlanExecutionFailure::Operation(failure)) => {
                            return Err(invalid_state(format!(
                                "note deletion refused: {failure:?}"
                            )));
                        }
                    }
                }
                writer
                    .execute(SqlStatement::new(
                        "INSERT INTO note_stream_floors(namespace,stream,floor_seq) VALUES (?1,?2,?3) \
                         ON CONFLICT(namespace,stream) DO UPDATE SET floor_seq=excluded.floor_seq",
                        prefix_params,
                    ).labelled("stream-truncate-floor"))
                    .await?;
                let cleared = writer
                    .execute(
                        SqlStatement::new(
                            "DELETE FROM note_stream_truncations WHERE namespace=?1 AND stream=?2",
                            vec![
                                SqlValue::Text(namespace.into()),
                                SqlValue::Text(stream.clone()),
                            ],
                        )
                        .labelled("stream-truncate-clear-permit"),
                    )
                    .await?;
                if cleared != 1 {
                    return Err(PreparedAtomicError::Storage(StorageError::Internal(
                        "stream truncation permit disappeared".into(),
                    )));
                }
                Ok((
                    json!({"removed": removed, "floor_seq": before, "head_seq": head}),
                    effects,
                ))
            })
        });
        let access = self.sql();
        match run_prepared_atomic_unit(access.as_ref(), op).await? {
            PreparedAtomicOutcome::RolledBack(error) => Err(error),
            PreparedAtomicOutcome::Committed { value, post_commit } => {
                apply_post_commit_effects_with_report(self, token, post_commit).await?;
                Ok(value)
            }
        }
    }
}
