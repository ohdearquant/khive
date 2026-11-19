//! Existing-record attachment publication; no external I/O while holding the writer.

use std::any::Any;

use khive_db::stores::attachment::attachment_upsert_statement;
use khive_storage::{
    Attachment, AttachmentSubstrate, NewAttachment, SqlStatement, SqlValue, StorageCapability,
    StorageError,
};
use uuid::Uuid;

use crate::{KhiveRuntime, NamespaceToken, Resolved, RuntimeError, RuntimeResult};

impl KhiveRuntime {
    /// Publish one role on a live main-backend entity or note.
    ///
    /// Bytes are confirmed before writer admission. The owner snapshot, replacement
    /// decision and attachment row share one transaction. A refused row write can
    /// leave unreferenced bytes, but never a newly published row pointing at a failed put.
    pub async fn publish_record_attachment(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        role: String,
        bytes: Vec<u8>,
        media_type: Option<String>,
    ) -> RuntimeResult<(Attachment, bool)> {
        if self.backend_id().as_str() != crate::BackendId::MAIN {
            return Err(StorageError::Unsupported {
                capability: StorageCapability::Attachments,
                operation: "attach".into(),
                message: format!(
                    "attachment publication requires the canonical main backend; runtime backend {:?} cannot publish atomically there",
                    self.backend_id().as_str()
                ),
            }
            .into());
        }
        // Retain the canonical attachment accessor's capability checks.
        drop(self.attachments()?);
        khive_storage::attachment::validate_attachment_role(&role)?;
        let (substrate, namespace, version, updated_at, entity_type) =
            match self.resolve_by_id(token, id).await? {
                Some(Resolved::Entity(entity)) => (
                    AttachmentSubstrate::Entity,
                    entity.namespace,
                    entity.version,
                    entity.updated_at,
                    entity.entity_type,
                ),
                Some(Resolved::Note(note)) => (
                    AttachmentSubstrate::Note,
                    note.namespace,
                    note.version,
                    note.updated_at,
                    None,
                ),
                _ => return Err(RuntimeError::NotFound(format!("attachment record {id}"))),
            };
        if (role == "fann-network" && entity_type.as_deref() == Some("moodboard_model"))
            || (role == "content"
                && matches!(
                    entity_type.as_deref(),
                    Some("visual_asset" | "moodboard_model")
                ))
        {
            return Err(RuntimeError::InvalidInput(format!(
                "attachment role {role:?} on this record is owned by the moodboard pack; use its publication verbs"
            )));
        }

        let size = bytes.len() as u64;
        let content_ref = self.require_blob_store()?.put(bytes).await?;
        let attachment = Attachment::from_new(
            id,
            substrate,
            NewAttachment {
                role: role.clone(),
                content_ref,
                media_type,
                size_bytes: Some(size),
            },
            chrono::Utc::now().timestamp_micros(),
        );
        let upsert = attachment_upsert_statement(&attachment)?;
        let table = match substrate {
            AttachmentSubstrate::Entity => "entities",
            AttachmentSubstrate::Note => "notes",
        };
        let owner = SqlStatement::new(
            format!(
                "SELECT 1 FROM {table} WHERE id = ?1 AND namespace = ?2 \
                 AND version = ?3 AND updated_at = ?4 AND deleted_at IS NULL"
            ),
            vec![
                SqlValue::Text(id.to_string()),
                SqlValue::Text(namespace),
                SqlValue::Integer(version),
                SqlValue::Integer(updated_at),
            ],
        )
        .labelled("attach-owner-snapshot");
        let prior = SqlStatement::new(
            "SELECT substrate FROM attachments WHERE record_uuid = ?1 AND role = ?2",
            vec![SqlValue::Text(id.to_string()), SqlValue::Text(role)],
        )
        .labelled("attach-replacement");
        let result = self
            .sql()
            .atomic_unit(Box::new(move |writer| {
                Box::pin(async move {
                    if writer.query_row(owner).await?.is_none() {
                        return Err(StorageError::InvalidInput {
                            capability: StorageCapability::Attachments,
                            operation: "attach".into(),
                            message: format!(
                                "attachment record {id} changed or was deleted before publication"
                            ),
                        });
                    }
                    let old = writer.query_row(prior).await?;
                    if old
                        .as_ref()
                        .is_some_and(|row| row.text("substrate").ok() != Some(substrate.as_str()))
                    {
                        return Err(StorageError::InvalidInput {
                            capability: StorageCapability::Attachments,
                            operation: "attach".into(),
                            message: format!("attachment record {id} has a conflicting substrate"),
                        });
                    }
                    writer.execute(upsert).await?;
                    Ok(Box::new(old.is_some()) as Box<dyn Any + Send>)
                })
            }))
            .await?;
        let replaced = *result.downcast::<bool>().map_err(|_| {
            RuntimeError::Internal("attach transaction returned an unexpected result".into())
        })?;
        Ok((attachment, replaced))
    }
}
