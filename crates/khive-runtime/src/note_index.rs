//! Index writes for a note revision must not outlive that revision.

use std::any::Any;

use khive_storage::note::Note;
use khive_storage::{AtomicUnitOp, SqlStatement, SqlValue};

use crate::{KhiveRuntime, RuntimeError, RuntimeResult};

impl KhiveRuntime {
    pub(crate) async fn apply_note_index_revision(
        &self,
        note: &Note,
        statements: Vec<SqlStatement>,
    ) -> RuntimeResult<bool> {
        self.apply_note_revision_statements(note, statements, false)
            .await
    }

    pub(crate) async fn publish_note_vector_revision(
        &self,
        token: &crate::NamespaceToken,
        note: &Note,
        model_name: &str,
        vector: &[f32],
    ) -> RuntimeResult<bool> {
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
        self.apply_note_index_revision(note, statements).await
    }

    pub(crate) async fn compensate_note_creation(&self, note: &Note) -> bool {
        let mut statements = khive_db::stores::text::delete_document_statements(
            "fts_notes",
            &note.namespace,
            note.id,
        )
        .to_vec();
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
        match self
            .apply_note_revision_statements(note, statements, true)
            .await
        {
            Ok(removed) => removed,
            Err(error) => {
                tracing::warn!(note_id = %note.id, %error, "note creation compensation failed");
                false
            }
        }
    }

    async fn apply_note_revision_statements(
        &self,
        note: &Note,
        statements: Vec<SqlStatement>,
        purge_vectors: bool,
    ) -> RuntimeResult<bool> {
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
                    return Ok(Box::new(false) as Box<dyn Any + Send>);
                }
                if purge_vectors {
                    purge.apply(writer).await?;
                }
                for statement in statements {
                    writer.execute(statement).await?;
                }
                Ok(Box::new(true) as Box<dyn Any + Send>)
            })
        });
        self.sql()
            .atomic_unit(op)
            .await?
            .downcast::<bool>()
            .map(|result| *result)
            .map_err(|_| RuntimeError::Internal("invalid note index outcome".into()))
    }
}
