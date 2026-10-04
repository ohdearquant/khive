//! Revision-guarded note indexing with caller-visible partial model failures.
use super::{note_embedding_text_ref, note_fts_document, EmbeddingModelPlan};
use crate::error::{RuntimeError, RuntimeResult};
use crate::retrieval::EmbeddingTruncationReport;
use crate::runtime::{KhiveRuntime, NamespaceToken};
use khive_storage::note::Note;
use khive_storage::types::SqlValue;
use khive_storage::SqlStatement;
use uuid::Uuid;

#[derive(Default)]
pub(super) struct NoteReindexOutcome {
    pub(super) truncation: EmbeddingTruncationReport,
    failures: Vec<String>,
}

impl NoteReindexOutcome {
    pub(super) fn error(&self, id: Uuid) -> Option<RuntimeError> {
        (!self.failures.is_empty()).then(|| {
            RuntimeError::Internal(format!(
                "note {id} reindex incomplete after text indexing; observed embedding_truncation \
             truncated={} discarded_bytes={}; {}",
                self.truncation.truncated,
                self.truncation.discarded_bytes,
                self.failures.join("; ")
            ))
        })
    }

    fn into_result(self, id: Uuid) -> RuntimeResult<EmbeddingTruncationReport> {
        match self.error(id) {
            Some(error) => Err(error),
            None => Ok(self.truncation),
        }
    }
}

impl KhiveRuntime {
    /// Re-upsert FTS5 and kind-eligible vectors, removing excluded-model rows.
    ///
    /// Excluded-model cleanup is revision-guarded and fail-closed. Embedding
    /// eligible models continues after failures and reports incomplete indexing.
    pub(crate) async fn reindex_note(
        &self,
        token: &NamespaceToken,
        note: &khive_storage::note::Note,
    ) -> RuntimeResult<crate::retrieval::EmbeddingTruncationReport> {
        let embedding_plan = EmbeddingModelPlan::capture(self);
        self.reindex_note_with_plan(token, note, &embedding_plan)
            .await
    }

    pub(super) async fn reindex_note_with_plan(
        &self,
        token: &NamespaceToken,
        note: &Note,
        embedding_plan: &EmbeddingModelPlan,
    ) -> RuntimeResult<EmbeddingTruncationReport> {
        self.reindex_note_outcome(token, note, embedding_plan)
            .await?
            .into_result(note.id)
    }

    pub(super) async fn reindex_note_outcome(
        &self,
        token: &NamespaceToken,
        note: &khive_storage::note::Note,
        embedding_plan: &EmbeddingModelPlan,
    ) -> RuntimeResult<NoteReindexOutcome> {
        let statements = khive_db::stores::text::delete_document_statements(
            "fts_notes",
            &note.namespace,
            note.id,
        )
        .into_iter()
        .chain(khive_db::stores::text::insert_document_statements(
            "fts_notes",
            &note_fts_document(note),
        ))
        .collect();
        if !self.apply_note_index_revision(note, statements).await? {
            return Ok(NoteReindexOutcome::default());
        }
        let mut result = NoteReindexOutcome::default();
        let selected_models = self.embedding_models_for_note_kind(&note.kind);
        // A kind policy can narrow after an earlier revision wrote vectors to
        // every model. Remove those stale rows from every excluded table in
        // the captured plan, under the same note-revision fence as FTS writes.
        for model_name in embedding_plan
            .model_names()
            .iter()
            .filter(|name| !selected_models.contains(*name))
        {
            // The vector table is created lazily. A missing table has no old
            // row to remove, but preparing it also makes the guarded DML safe.
            self.vectors_for_model(token, model_name)?;
            let table = format!("vec_{}", crate::config::sanitize_key(model_name));
            let model_key = table
                .strip_prefix("vec_")
                .expect("runtime vector tables use the vec_ prefix");
            let subject = note.id.to_string();
            // A selected and an excluded model may sanitize to the same table
            // key. Check the stored model before touching either row or sidecar.
            let statements = vec![
                SqlStatement {
                    sql: format!(
                        "INSERT INTO ann_write_log \
                     (namespace, embedding_model, kind, field, subject_id, op) \
                     SELECT namespace, embedding_model, kind, field, subject_id, 'delete' \
                     FROM {table} WHERE subject_id=?1 AND namespace=?2 AND embedding_model=?3"
                    ),
                    params: vec![
                        SqlValue::Text(subject.clone()),
                        SqlValue::Text(note.namespace.clone()),
                        SqlValue::Text(model_name.clone()),
                    ],
                    label: Some("note-reindex-excluded-log-delete".into()),
                },
                SqlStatement {
                    sql: format!(
                        "DELETE FROM vector_provenance \
                         WHERE model_key=?1 AND subject_id=?2 \
                         AND EXISTS (SELECT 1 FROM {table} \
                                     WHERE subject_id=?2 AND namespace=?3 AND embedding_model=?4)"
                    ),
                    params: vec![
                        SqlValue::Text(model_key.to_string()),
                        SqlValue::Text(subject.clone()),
                        SqlValue::Text(note.namespace.clone()),
                        SqlValue::Text(model_name.clone()),
                    ],
                    label: Some("note-reindex-excluded-provenance-delete".into()),
                },
                SqlStatement {
                    sql: format!(
                        "DELETE FROM {table} \
                         WHERE subject_id=?1 AND namespace=?2 AND embedding_model=?3"
                    ),
                    params: vec![
                        SqlValue::Text(subject),
                        SqlValue::Text(note.namespace.clone()),
                        SqlValue::Text(model_name.clone()),
                    ],
                    label: Some("note-reindex-excluded-vector-delete".into()),
                },
            ];
            if !self.apply_note_index_revision(note, statements).await? {
                return Ok(result);
            }
        }
        for model_name in embedding_plan
            .model_names()
            .iter()
            .filter(|name| selected_models.contains(*name))
        {
            match self
                .embed_document_with_model_outcome_for_token(
                    token,
                    model_name,
                    note_embedding_text_ref(note),
                )
                .await
            {
                Ok(outcome) => {
                    result.truncation.observe(&outcome);
                    match self.vectors_for_model(token, model_name) {
                        Ok(_) => {
                            if outcome.vector.iter().any(|value| !value.is_finite()) {
                                tracing::warn!(model = model_name, id = %note.id, "reindex_note: non-finite vector, skipping model");
                                result.failures.push(format!(
                                    "model {model_name} vector validation: non-finite output"
                                ));
                                continue;
                            }
                            let table = format!("vec_{}", crate::config::sanitize_key(model_name));
                            let statements = crate::atomic_message::vector_insert_statements(
                                &table,
                                &note.namespace,
                                note.id,
                                "note.content",
                                model_name,
                                &outcome.vector,
                                "note-reindex",
                            )
                            .into_iter()
                            .map(|planned| planned.statement)
                            .collect();
                            if let Err(e) = self.apply_note_index_revision(note, statements).await {
                                tracing::warn!(
                                    model = model_name,
                                    id = %note.id,
                                    "reindex_note: vector insert failed, skipping model: {e}"
                                );
                                result
                                    .failures
                                    .push(format!("model {model_name} vector publication: {e}"));
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                model = model_name,
                                id = %note.id,
                                "reindex_note: could not access vector store for model, skipping: {e}"
                            );
                            result
                                .failures
                                .push(format!("model {model_name} vector store: {e}"));
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        model = model_name,
                        id = %note.id,
                        "reindex_note: embed failed for model, skipping: {e}"
                    );
                    result
                        .failures
                        .push(format!("model {model_name} embedding: {e}"));
                }
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
#[path = "note_reindex_tests.rs"]
mod tests;
