//! Index writes for a note revision must not outlive that revision.

use std::any::Any;

use khive_storage::note::Note;
use khive_storage::{AtomicUnitOp, SqlStatement, SqlValue};

use crate::{KhiveRuntime, RuntimeError, RuntimeResult};

struct NoteIndexRevision {
    applied: bool,
    ann_write_log_seq: Option<i64>,
}

impl KhiveRuntime {
    pub(crate) async fn apply_note_index_revision(
        &self,
        note: &Note,
        statements: Vec<SqlStatement>,
    ) -> RuntimeResult<bool> {
        Ok(self
            .apply_note_revision_statements(note, statements, false, false)
            .await?
            .applied)
    }

    pub(crate) async fn publish_note_vector_revision(
        &self,
        token: &crate::NamespaceToken,
        note: &Note,
        model_name: &str,
        vector: &[f32],
    ) -> RuntimeResult<bool> {
        Ok(self
            .publish_note_vector_revision_inner(token, note, model_name, vector, false)
            .await?
            .applied)
    }

    /// Return the exact upsert-log sequence from the same transaction that
    /// published this note's vector. A version-guard miss writes no vector and
    /// returns `None`; callers must never substitute a later `MAX(seq)` read.
    pub(crate) async fn publish_note_vector_revision_with_seq(
        &self,
        token: &crate::NamespaceToken,
        note: &Note,
        model_name: &str,
        vector: &[f32],
    ) -> RuntimeResult<Option<u64>> {
        let revision = self
            .publish_note_vector_revision_inner(token, note, model_name, vector, true)
            .await?;
        if !revision.applied {
            return Ok(None);
        }
        let seq = revision
            .ann_write_log_seq
            .and_then(|seq| u64::try_from(seq).ok())
            .filter(|seq| *seq > 0)
            .ok_or_else(|| {
                RuntimeError::Internal(
                    "note vector revision committed without a positive ANN log sequence".into(),
                )
            })?;
        Ok(Some(seq))
    }

    async fn publish_note_vector_revision_inner(
        &self,
        token: &crate::NamespaceToken,
        note: &Note,
        model_name: &str,
        vector: &[f32],
        capture_ann_seq: bool,
    ) -> RuntimeResult<NoteIndexRevision> {
        let (model_name, dimensions) = self.vector_model_metadata(model_name)?;
        self.backend().vectors_for_namespace(
            &crate::config::sanitize_key(&model_name),
            &model_name,
            dimensions,
            token.namespace().as_str(),
        )?;
        if vector.len() != dimensions {
            return Err(RuntimeError::Storage(
                khive_storage::StorageError::InvalidInput {
                    capability: khive_storage::StorageCapability::Vectors,
                    operation: "vec_insert".into(),
                    message: format!(
                        "expected {dimensions} vector dimensions, got {}",
                        vector.len()
                    ),
                },
            ));
        }
        if let Some(index) = vector.iter().position(|value| !value.is_finite()) {
            return Err(crate::atomic_message::non_finite_vector_error(
                index,
                vector[index],
            ));
        }
        let statements = crate::atomic_message::vector_insert_statements(
            &format!("vec_{}", crate::config::sanitize_key(&model_name)),
            &note.namespace,
            note.id,
            "note.content",
            &model_name,
            vector,
            "note-revision-vector",
        )
        .into_iter()
        .map(|planned| planned.statement)
        .collect();
        self.apply_note_revision_statements(note, statements, false, capture_ann_seq)
            .await
    }

    pub(crate) async fn compensate_note_creation(&self, note: &Note) -> bool {
        match self.compensate_note_creation_inner(note, false).await {
            Ok(removed) => removed,
            Err(error) => {
                tracing::warn!(note_id = %note.id, %error, "note creation compensation failed");
                false
            }
        }
    }

    /// Roll back a partially linked note. Incident edges and the note row are
    /// removed by one writer transaction, or both remain for reconciliation.
    pub(crate) async fn compensate_note_creation_with_edges(
        &self,
        note: &Note,
    ) -> RuntimeResult<bool> {
        self.compensate_note_creation_inner(note, true).await
    }

    async fn compensate_note_creation_inner(
        &self,
        note: &Note,
        purge_edges: bool,
    ) -> RuntimeResult<bool> {
        let mut statements = khive_db::stores::text::delete_document_statements(
            "fts_notes",
            &note.namespace,
            note.id,
        )
        .to_vec();
        if purge_edges {
            statements.push(khive_db::stores::graph::purge_incident_edges_statement(
                note.id,
            ));
        }
        statements.push(crate::note_write::statement(
            "DELETE FROM notes WHERE namespace=?1 AND id=?2",
            vec![
                SqlValue::Text(note.namespace.clone()),
                SqlValue::Text(note.id.to_string()),
            ],
        ));
        statements.push(
            khive_db::stores::attachment::delete_record_attachments_statement(
                note.id,
                khive_storage::attachment::AttachmentSubstrate::Note,
            ),
        );
        Ok(self
            .apply_note_revision_statements(note, statements, true, false)
            .await?
            .applied)
    }

    async fn apply_note_revision_statements(
        &self,
        note: &Note,
        statements: Vec<SqlStatement>,
        purge_vectors: bool,
        capture_ann_seq: bool,
    ) -> RuntimeResult<NoteIndexRevision> {
        let namespace = note.namespace.clone();
        let id = note.id.to_string();
        let version = note.version;
        let purge = crate::note_write::NoteVectors::new(namespace.clone(), note.id);
        let op: AtomicUnitOp = Box::new(move |writer| {
            Box::pin(async move {
                let current = writer.query_scalar(crate::note_write::statement(
                "SELECT version FROM notes WHERE namespace=?1 AND id=?2 AND deleted_at IS NULL",
                vec![SqlValue::Text(namespace), SqlValue::Text(id)],
            )).await?;
                if !matches!(current, Some(SqlValue::Integer(current)) if current == version) {
                    return Ok(Box::new(NoteIndexRevision {
                        applied: false,
                        ann_write_log_seq: None,
                    }) as Box<dyn Any + Send>);
                }
                if purge_vectors {
                    purge.apply(writer).await?;
                }
                for statement in statements {
                    writer.execute(statement).await?;
                }
                let ann_write_log_seq = if capture_ann_seq {
                    match writer
                        .query_scalar(crate::note_write::statement(
                            "SELECT last_insert_rowid()",
                            Vec::new(),
                        ))
                        .await?
                    {
                        Some(SqlValue::Integer(seq)) => Some(seq),
                        _ => None,
                    }
                } else {
                    None
                };
                Ok(Box::new(NoteIndexRevision {
                    applied: true,
                    ann_write_log_seq,
                }) as Box<dyn Any + Send>)
            })
        });
        self.sql()
            .atomic_unit(op)
            .await?
            .downcast::<NoteIndexRevision>()
            .map(|result| *result)
            .map_err(|_| RuntimeError::Internal("invalid note index outcome".into()))
    }
}
